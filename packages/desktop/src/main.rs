use std::{collections::HashMap, env, sync::Arc};

use clap::Parser;
use providers::{
  config::ProviderConfig, provider_manager::init_provider_manager,
};
use serde::Serialize;
use tauri::{
  AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder, Window,
};
use tokio::{
  sync::{
    mpsc::{self, UnboundedSender},
    Mutex,
  },
  task,
};
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::EnvFilter;

use crate::{
  cli::{Cli, CliCommand},
  monitors::get_monitors_str,
  providers::provider_manager::ProviderManager,
  sys_tray::setup_sys_tray,
  util::window_ext::WindowExt,
};

mod cli;
mod monitors;
mod providers;
mod sys_tray;
mod user_config;
mod util;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct OpenWindowArgs {
  window_id: String,
  args: HashMap<String, String>,
  env: HashMap<String, String>,
}

struct OpenWindowArgsMap(Arc<Mutex<HashMap<String, OpenWindowArgs>>>);

#[tauri::command]
fn read_config_file(
  config_path_override: Option<&str>,
  app_handle: AppHandle,
) -> anyhow::Result<String, String> {
  user_config::read_file(config_path_override, app_handle)
    .map_err(|err| err.to_string())
}

#[tauri::command]
async fn get_open_window_args(
  window_label: String,
  open_window_args_map: State<'_, OpenWindowArgsMap>,
) -> anyhow::Result<Option<OpenWindowArgs>, String> {
  Ok(
    open_window_args_map
      .0
      .lock()
      .await
      .get(&window_label)
      .map(|open_args| open_args.clone()),
  )
}

#[tauri::command]
async fn listen_provider(
  config_hash: String,
  config: ProviderConfig,
  tracked_access: Vec<String>,
  provider_manager: State<'_, ProviderManager>,
) -> anyhow::Result<(), String> {
  provider_manager
    .create(config_hash, config, tracked_access)
    .await
    .map_err(|err| err.to_string())
}

#[tauri::command]
async fn unlisten_provider(
  config_hash: String,
  provider_manager: State<'_, ProviderManager>,
) -> anyhow::Result<(), String> {
  provider_manager
    .destroy(config_hash)
    .await
    .map_err(|err| err.to_string())
}

/// Tauri's implementation of `always_on_top` places the window above
/// all normal windows (but not the MacOS menu bar). The following instead
/// sets the z-order of the window to be above the menu bar.
#[tauri::command]
fn set_always_on_top(window: Window) -> anyhow::Result<(), String> {
  #[cfg(target_os = "macos")]
  let res = window.set_above_menu_bar();

  #[cfg(not(target_os = "macos"))]
  let res = window.set_always_on_top(true);

  res.map_err(|err| err.to_string())
}

#[tauri::command]
fn set_skip_taskbar(
  window: Window,
  skip: bool,
) -> anyhow::Result<(), String> {
  window
    .set_skip_taskbar(skip)
    .map_err(|err| err.to_string())?;

  #[cfg(target_os = "windows")]
  window
    .set_tool_window(skip)
    .map_err(|err| err.to_string())?;

  Ok(())
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_env_filter(
      EnvFilter::from_env("LOG_LEVEL")
        .add_directive(LevelFilter::INFO.into()),
    )
    .init();

  tauri::async_runtime::set(tokio::runtime::Handle::current());

  tauri::Builder::default()
    .setup(|app| {
      let cli = Cli::parse();

      // Since most Tauri plugins and setup is not needed for the
      // `monitors` CLI command, the setup is conditional based on
      // the CLI command.
      match cli.command {
        CliCommand::Monitors { print0 } => {
          let monitors_str = get_monitors_str(app, print0);
          cli::print_and_exit(monitors_str);
          Ok(())
        }
        CliCommand::Open { window_id, args } => {
          let (tx, mut rx) = mpsc::unbounded_channel::<OpenWindowArgs>();
          let tx_clone = tx.clone();

          // If this is not the first instance of the app, this will emit
          // to the original instance and exit immediately.
          app.handle().plugin(tauri_plugin_single_instance::init(
            move |_, args, _| {
              let cli = Cli::parse_from(args);

              // CLI command is guaranteed to be an open command here.
              if let CliCommand::Open { window_id, args } = cli.command {
                emit_open_args(window_id, args, tx.clone());
              }
            },
          ))?;

          emit_open_args(window_id, args, tx_clone);

          app.handle().plugin(tauri_plugin_shell::init())?;
          app.handle().plugin(tauri_plugin_http::init())?;
          app.handle().plugin(tauri_plugin_dialog::init())?;

          // Add application icon to system tray.
          setup_sys_tray(app)?;

          init_provider_manager(app);

          let args_map = OpenWindowArgsMap(Default::default());
          let args_map_ref = args_map.0.clone();
          app.manage(args_map);

          let app_handle = app.handle().clone();

          // Prevent the app icon from showing up in the dock on MacOS.
          #[cfg(target_os = "macos")]
          app.set_activation_policy(tauri::ActivationPolicy::Accessory);

          // Handle creation of new windows (both from the initial and
          // subsequent instances of the application)
          _ = task::spawn(async move {
            let window_count = Arc::new(Mutex::new(0));

            while let Some(open_args) = rx.recv().await {
              let mut window_count = window_count.lock().await;
              *window_count += 1;

              info!(
                "Creating window #{} '{}' with args: {:#?}",
                window_count, open_args.window_id, open_args.args
              );

              // Window label needs to be globally unique. Hence add a
              // prefix with the window count to handle cases where
              // multiple of the same window are opened.
              let window_label =
                format!("{}-{}", window_count, &open_args.window_id);

              let window = WebviewWindowBuilder::new(
                &app_handle,
                &window_label,
                WebviewUrl::default(),
              )
              .title(format!("Zebar - {}", open_args.window_id))
              .inner_size(500., 500.)
              .focused(false)
              .skip_taskbar(true)
              .visible_on_all_workspaces(true)
              .transparent(true)
              .shadow(false)
              .decorations(false)
              .resizable(false)
              .build()
              .unwrap();

              _ = window.eval(&format!(
                "window.__ZEBAR_OPEN_ARGS={}",
                serde_json::to_string(&open_args).unwrap()
              ));

              // Tauri's `skip_taskbar` option isn't 100% reliable, so we
              // also set the window as a tool window.
              #[cfg(target_os = "windows")]
              let _ = window.as_ref().window().set_tool_window(true);

              let mut args_map = args_map_ref.lock().await;
              args_map.insert(window_label, open_args);
            }
          });

          Ok(())
        }
      }
    })
    .invoke_handler(tauri::generate_handler![
      read_config_file,
      get_open_window_args,
      listen_provider,
      unlisten_provider,
      set_always_on_top,
      set_skip_taskbar
    ])
    .run(tauri::generate_context!())
    .expect("Failed to build Tauri application.");
}

/// Create and emit `OpenWindowArgs` to a channel.
fn emit_open_args(
  window_id: String,
  args: Option<Vec<(String, String)>>,
  tx: UnboundedSender<OpenWindowArgs>,
) {
  let open_args = OpenWindowArgs {
    window_id,
    args: args.unwrap_or(vec![]).into_iter().collect(),
    env: env::vars().collect(),
  };

  if let Err(err) = tx.send(open_args.clone()) {
    info!("Failed to emit window's open args: {}", err);
  };
}
