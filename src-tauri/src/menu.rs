// 应用菜单：应用（更新 / 窗口 / 退出）/ 内核 / 帮助 三个下拉子菜单；
// 加速键按操作系统映射：CmdOrCtrl 在 macOS 为 ⌘、Windows/Linux 为 Ctrl。
use tauri::menu::{IsMenuItem, Menu, MenuBuilder, MenuItemBuilder, MenuEvent, SubmenuBuilder};
use tauri::{AppHandle, Manager, Window, Wry};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::config;
use crate::server::{self, KernelStatus, Shared};
use crate::state::{AppState, SuperCommand};

/// 全屏切换快捷键按平台取惯例键：macOS ^⌘F，Windows/Linux F11。
fn fullscreen_accel() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Ctrl+Cmd+F"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "F11"
    }
}

pub fn build_menu(app: &AppHandle<Wry>) -> tauri::Result<Menu<Wry>> {
    // 应用子菜单（macOS 上标题自动显示为应用名）：更新 / 窗口 / 退出。
    let check_updates = MenuItemBuilder::with_id("check-updates", "检查更新…")
        .accelerator("CmdOrCtrl+U")
        .build(app)?;
    let restore_kernel = MenuItemBuilder::with_id("restore-bundled-kernel", "恢复内置内核").build(app)?;
    let minimize = MenuItemBuilder::with_id("minimize-window", "最小化")
        .accelerator("CmdOrCtrl+M")
        .build(app)?;
    let fullscreen = MenuItemBuilder::with_id("toggle-fullscreen", "切换全屏")
        .accelerator(fullscreen_accel())
        .build(app)?;
    let toggle_sidebar = MenuItemBuilder::with_id("toggle-sidebar", "切换侧边栏")
        .accelerator("CmdOrCtrl+B")
        .build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出 DeepSeek Harness")
        .accelerator("CmdOrCtrl+Q")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, "应用")
        .item(&check_updates)
        .item(&restore_kernel)
        .separator()
        .item(&minimize)
        .item(&fullscreen)
        .item(&toggle_sidebar)
        .separator()
        .item(&quit)
        .build()?;

    // 内核子菜单：工作目录 / 浏览器 / 重启。
    let change_workspace = MenuItemBuilder::with_id("change-workspace", "更改工作目录…")
        .accelerator("CmdOrCtrl+Shift+O")
        .build(app)?;
    let open_browser = MenuItemBuilder::with_id("open-in-browser", "在浏览器中打开").build(app)?;
    let restart = MenuItemBuilder::with_id("restart-kernel", "重启内核")
        .accelerator("CmdOrCtrl+R")
        .build(app)?;
    let kernel_menu = SubmenuBuilder::new(app, "内核")
        .item(&change_workspace)
        .item(&open_browser)
        .separator()
        .item(&restart)
        .build()?;

    // 帮助子菜单：诊断 / 日志。
    let diagnostics = MenuItemBuilder::with_id("copy-diagnostics", "复制诊断信息").build(app)?;
    let open_logs = MenuItemBuilder::with_id("open-logs", "打开日志目录").build(app)?;
    let help_menu = SubmenuBuilder::new(app, "帮助")
        .item(&diagnostics)
        .item(&open_logs)
        .build()?;

    let top: Vec<&dyn IsMenuItem<Wry>> = vec![&app_menu, &kernel_menu, &help_menu];
    MenuBuilder::new(app).items(&top).build()
}

pub fn handle_menu_event(app: &AppHandle<Wry>, event: MenuEvent) {
    match event.id().as_ref() {
        "change-workspace" => change_workspace(app),
        "open-in-browser" => open_in_browser(app),
        "restart-kernel" => send_command(app, SuperCommand::RestartNow),
        "check-updates" => crate::update::check_and_prompt(app, true),
        "restore-bundled-kernel" => restore_bundled_kernel(app),
        "minimize-window" => minimize_window(app),
        "toggle-fullscreen" => toggle_fullscreen(app),
        "toggle-sidebar" => toggle_sidebar(app),
        "copy-diagnostics" => copy_diagnostics(app),
        "open-logs" => open_logs(app),
        "quit" => send_command(app, SuperCommand::Shutdown),
        _ => {}
    }
}

fn send_command(app: &AppHandle<Wry>, command: SuperCommand) {
    let state = app.state::<AppState>();
    let _ = state.tx.send(command);
}

/// 主窗口（内核就绪后创建）；仅 splash 阶段为 None。
/// 多 webview 重构后 "main" 是窗口 label（内含 kernel/sidebar 两个 webview）。
fn main_window(app: &AppHandle<Wry>) -> Option<Window<Wry>> {
    app.get_window("main")
}

fn minimize_window(app: &AppHandle<Wry>) {
    if let Some(window) = main_window(app) {
        let _ = window.minimize();
    }
}

fn toggle_fullscreen(app: &AppHandle<Wry>) {
    if let Some(window) = main_window(app) {
        let fullscreen = window.is_fullscreen().unwrap_or(false);
        let _ = window.set_fullscreen(!fullscreen);
    }
}

fn toggle_sidebar(app: &AppHandle<Wry>) {
    let _ = crate::sidebar::toggle_sidebar(app.clone());
}

fn change_workspace(app: &AppHandle<Wry>) {
    let picked = app.dialog().file().blocking_pick_folder();
    let Some(path) = picked else { return };
    let Ok(dir) = path.into_path() else { return };
    // load-modify-save：不能整体覆盖，否则会丢掉更新设置等其他字段。
    let mut settings = config::load(app);
    settings.workspace_root = Some(dir.to_string_lossy().into_owned());
    if let Err(error) = config::save(app, &settings) {
        log::error!("保存工作目录设置失败：{}", error);
    }
    send_command(app, SuperCommand::SetWorkspace(dir));
}

/// 恢复内置内核：确认后删除更新覆盖层并重启内核。
fn restore_bundled_kernel(app: &AppHandle<Wry>) {
    let confirmed = app
        .dialog()
        .message("将删除已更新的内核运行时并恢复应用内置版本，随后重启内核。继续？")
        .title("恢复内置内核")
        .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
            String::from("恢复"),
            String::from("取消"),
        ))
        .blocking_show();
    if confirmed {
        crate::update::restore_bundled_kernel(app);
    }
}

fn open_in_browser(app: &AppHandle<Wry>) {
    let state = app.state::<AppState>();
    let status = state.shared.status.lock().unwrap().clone();
    if let KernelStatus::Ready { url } = status {
        let _ = app.opener().open_url(url, None::<&str>);
    }
}

fn open_logs(app: &AppHandle<Wry>) {
    if let Ok(dir) = app.path().app_log_dir() {
        let _ = app.opener().open_path(dir.display().to_string(), None::<&str>);
    }
}

fn status_summary(status: &KernelStatus) -> String {
    match status {
        KernelStatus::Starting { attempt } => format!("启动中（第 {} 次）", attempt),
        KernelStatus::Restarting { attempt } => format!("重启中（第 {} 次）", attempt),
        KernelStatus::Ready { url } => format!("就绪 {}", url),
        KernelStatus::Failed { error } => format!("失败：{}", error),
    }
}

fn copy_diagnostics(app: &AppHandle<Wry>) {
    let state = app.state::<AppState>();
    let shared: &Shared = &state.shared;
    let status = shared.status.lock().unwrap().clone();
    let port = shared
        .port
        .lock()
        .unwrap()
        .map(|p| p.to_string())
        .unwrap_or_else(|| String::from("-"));
    let workspace = shared.workspace.lock().unwrap().display().to_string();
    let node_version = server::node_version(shared);
    let dsh_version = server::dsh_version(shared);
    let dsh_home = std::env::var("DSH_HOME").unwrap_or_else(|_| {
        let mut home = config::home_dir();
        home.push(".dsh");
        home.display().to_string()
    });
    let log_dir = app
        .path()
        .app_log_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| String::from("未知"));
    let lines = vec![
        String::from("DeepSeek Harness 桌面版诊断信息"),
        format!("应用版本：{}", env!("CARGO_PKG_VERSION")),
        format!("内核状态：{}", status_summary(&status)),
        format!("监听端口：{}", port),
        format!("工作目录：{}", workspace),
        format!("DSH 版本：{}", dsh_version),
        format!("Node 版本：{}", node_version),
        format!("运行时来源：{}", shared.source.lock().unwrap().clone()),
        format!("更新：{}", crate::update::overlay_summary(app)),
        format!("最近检查：{}", crate::update::latest_summary(app)),
        format!("DSH_HOME：{}", dsh_home),
        format!("日志目录：{}", log_dir),
    ];
    let newline = char::from(10);
    let text = lines.join(newline.to_string().as_str());
    match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
        Ok(()) => log::info!("诊断信息已复制"),
        Err(error) => log::error!("复制诊断信息失败：{}", error),
    }
}
