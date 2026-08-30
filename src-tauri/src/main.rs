// dsh-desktop 入口：Tauri 壳。负责拉起本地 dsh web 内核、就绪后创建主 WebView 窗口、
// 单实例守卫、原生菜单与优雅退出。页面本身零 Tauri IPC（除 splash 的最小权限）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod menu;
mod server;
mod state;

use std::sync::Arc;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_log::{Target, TargetKind};

use server::{KernelRun, Shared};
use state::{AppState, SuperCommand};

fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .targets([
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::LogDir { file_name: None }),
                ])
                .build(),
        )
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![state::dsh_state, state::dsh_retry])
        .on_menu_event(menu::handle_menu_event)
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if matches!(window.label(), "main" | "splash") {
                    // 关窗即退出：优雅停止内核后由监督线程 app.exit。
                    api.prevent_close();
                    let app = window.app_handle();
                    let state = app.state::<AppState>();
                    let _ = state.tx.send(SuperCommand::Shutdown);
                }
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();

            // 内核运行时定位（开发覆盖 / 随包资源），并启动监督线程。
            let runtime = server::resolve_runtime(&handle)?;
            let args = vec![
                runtime.bin_js.to_string_lossy().into_owned(),
                String::from("--profile"),
                String::from("web"),
                String::from("--port"),
                String::from("0"),
                String::from("--no-open"),
            ];
            let workspace = config::current_workspace(&handle);
            let shared = Arc::new(Shared::new(
                runtime.program.clone(),
                runtime.bundle.clone(),
                workspace,
            ));
            let log_dir = handle.path().app_log_dir().map_err(|e| e.to_string())?;
            let run = KernelRun {
                program: runtime.program,
                args,
            };
            let tx = server::start_supervisor(handle.clone(), Arc::clone(&shared), run, log_dir.join("dsh.log"));
            app.manage(AppState { shared, tx: tx.clone() });

            // 外部信号（SIGTERM/SIGINT/SIGHUP）也走优雅退出：转发 Shutdown，由监督线程停内核后 app.exit。
            let signal_tx = tx.clone();
            std::thread::spawn(move || {
                use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
                use signal_hook::iterator::Signals;
                if let Ok(mut signals) = Signals::new([SIGTERM, SIGINT, SIGHUP]) {
                    for _signal in signals.forever() {
                        log::info!("收到终止信号，开始优雅退出");
                        let _ = signal_tx.send(SuperCommand::Shutdown);
                        break;
                    }
                }
            });

            // splash 窗口：唯一带 IPC 的窗口（最小权限）；主窗口就绪后创建且零 IPC。
            let _splash = WebviewWindowBuilder::new(app, "splash", WebviewUrl::App("splash.html".into()))
                .title("DeepSeek Harness")
                .inner_size(480.0, 320.0)
                .resizable(false)
                .center()
                .build()?;

            let app_menu = menu::build_menu(&handle)?;
            app.set_menu(app_menu)?;

            log::info!("dsh-desktop 启动完成");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("dsh-desktop 启动失败");
}
