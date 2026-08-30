// 应用菜单：工作目录 / 浏览器 / 重启内核 / 诊断 / 日志 / 退出。
use tauri::menu::{IsMenuItem, Menu, MenuBuilder, MenuItemBuilder, MenuEvent, PredefinedMenuItem};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::config::{self, AppSettings};
use crate::server::{self, KernelStatus, Shared};
use crate::state::{AppState, SuperCommand};

pub fn build_menu(app: &AppHandle<Wry>) -> tauri::Result<Menu<Wry>> {
    let change_workspace = MenuItemBuilder::with_id("change-workspace", "更改工作目录…")
        .accelerator("CmdOrCtrl+Shift+O")
        .build(app)?;
    let open_browser = MenuItemBuilder::with_id("open-in-browser", "在浏览器中打开").build(app)?;
    let restart = MenuItemBuilder::with_id("restart-kernel", "重启内核")
        .accelerator("CmdOrCtrl+R")
        .build(app)?;
    let diagnostics = MenuItemBuilder::with_id("copy-diagnostics", "复制诊断信息").build(app)?;
    let open_logs = MenuItemBuilder::with_id("open-logs", "打开日志目录").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出 DeepSeek Harness")
        .accelerator("CmdOrCtrl+Q")
        .build(app)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let items: Vec<&dyn IsMenuItem<Wry>> = vec![
        &change_workspace,
        &open_browser,
        &restart,
        &diagnostics,
        &open_logs,
        &separator,
        &quit,
    ];
    MenuBuilder::new(app).items(&items).build()
}

pub fn handle_menu_event(app: &AppHandle<Wry>, event: MenuEvent) {
    match event.id().as_ref() {
        "change-workspace" => change_workspace(app),
        "open-in-browser" => open_in_browser(app),
        "restart-kernel" => send_command(app, SuperCommand::RestartNow),
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

fn change_workspace(app: &AppHandle<Wry>) {
    let picked = app.dialog().file().blocking_pick_folder();
    let Some(path) = picked else { return };
    let Ok(dir) = path.into_path() else { return };
    let settings = AppSettings {
        workspace_root: Some(dir.to_string_lossy().into_owned()),
    };
    if let Err(error) = config::save(app, &settings) {
        log::error!("保存工作目录设置失败：{}", error);
    }
    send_command(app, SuperCommand::SetWorkspace(dir));
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
