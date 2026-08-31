// dsh-desktop 入口：Tauri 壳。负责拉起本地 dsh web 内核、就绪后创建主 WebView 窗口、
// 单实例守卫、原生菜单与优雅退出。页面本身零 Tauri IPC（除 splash 的最小权限）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod menu;
mod server;
mod sidebar;
mod state;
mod update;

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
            if let Some(window) = app.get_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            state::dsh_state,
            state::dsh_retry,
            update::update_state,
            update::update_begin,
            update::update_open_download,
            update::update_skip,
            update::update_dismiss,
            update::update_restart_kernel,
            update::update_check_now,
            sidebar::workspace_info,
            sidebar::fs_list,
            sidebar::fs_read,
            sidebar::fs_new_file,
            sidebar::fs_mkdir,
            sidebar::fs_rename,
            sidebar::fs_delete,
            sidebar::fs_copy_path,
            sidebar::fs_reveal,
            sidebar::search_workspace,
            sidebar::git_status,
            sidebar::git_add,
            sidebar::git_unstage,
            sidebar::git_discard,
            sidebar::git_commit,
            sidebar::git_push,
            sidebar::git_pull,
            sidebar::git_diff_file,
            sidebar::git_init,
            sidebar::git_branches,
            sidebar::git_checkout,
            sidebar::git_log,
            sidebar::git_commit_files,
            sidebar::git_commit_diff,
            sidebar::git_revert,
            sidebar::set_sidebar_width,
            sidebar::toggle_sidebar,
            sidebar::open_viewer
        ])
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
            if let WindowEvent::Resized { .. } = event {
                if window.label() == "main" {
                    server::layout_main_window(window);
                }
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();

            // 桥插件随包分发：装进 web profile（幂等），内核启动即带侧边栏数据源。
            sidebar::ensure_bridge_installed(&handle);

            // 内核运行时定位（开发覆盖 / 随包资源 / 更新覆盖层），并启动监督线程。
            let runtime = server::resolve_runtime(&handle)?;
            let workspace = config::current_workspace(&handle);
            let shared = Arc::new(Shared::new(
                runtime.program.clone(),
                runtime.bundle.clone(),
                workspace,
            ));
            *shared.source.lock().unwrap() = runtime.source.to_string();
            let log_dir = handle.path().app_log_dir().map_err(|e| e.to_string())?;

            // 每次内核（重）启动前重新解析运行时：更新覆盖层落位后重启即生效。
            let resolver_handle = handle.clone();
            let resolver_shared = Arc::clone(&shared);
            let resolver: server::RuntimeResolver = Arc::new(move || {
                let rt = server::resolve_runtime(&resolver_handle)?;
                *resolver_shared.program.lock().unwrap() = rt.program.clone();
                *resolver_shared.bundle.lock().unwrap() = rt.bundle.clone();
                *resolver_shared.source.lock().unwrap() = rt.source.to_string();
                let args = vec![
                    rt.bin_js.to_string_lossy().into_owned(),
                    String::from("--profile"),
                    String::from("web"),
                    String::from("--port"),
                    String::from("0"),
                    String::from("--no-open"),
                ];
                Ok(KernelRun { program: rt.program, args })
            });

            let tx = server::start_supervisor(handle.clone(), Arc::clone(&shared), resolver, log_dir.join("dsh.log"));
            app.manage(AppState { shared, tx: tx.clone() });
            app.manage(update::UpdateState::default());

            // 外部信号（SIGTERM/SIGINT/SIGHUP）也走优雅退出：转发 Shutdown，由监督线程停内核后 app.exit。
            #[cfg(unix)]
            {
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
            }

            // splash 窗口：唯一带 IPC 的窗口（最小权限）；主窗口就绪后创建且零 IPC。
            let _splash = WebviewWindowBuilder::new(app, "splash", WebviewUrl::App("splash.html".into()))
                .title("DeepSeek Harness")
                .inner_size(480.0, 320.0)
                .resizable(false)
                .center()
                .build()?;

            let app_menu = menu::build_menu(&handle)?;
            app.set_menu(app_menu)?;

            // 启动自动检查更新（内核就绪后触发；autoCheck 关闭则跳过）。
            update::start_auto_check(&handle);

            // 轮询内核会话工作区（桥插件），变化即刷新侧边栏。
            sidebar::start_bridge_poll(handle.clone());

            log::info!("dsh-desktop 启动完成");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("dsh-desktop 启动失败");
}
