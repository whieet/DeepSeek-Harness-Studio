// 应用全局状态与 splash 命令。
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use crate::server::{KernelStatus, Shared};

/// 监督线程指令。
pub enum SuperCommand {
    /// 优雅退出（杀内核后 app.exit）。
    Shutdown,
    /// 立即重启内核（杀当前内核并以全新状态启动）。
    RestartNow,
    /// 失败后手动重试。
    Retry,
    /// 更换工作目录后重启内核。
    SetWorkspace(PathBuf),
}

pub struct AppState {
    pub shared: Arc<Shared>,
    pub tx: Sender<SuperCommand>,
}

/// splash 页轮询的当前内核状态。
#[tauri::command]
pub fn dsh_state(state: tauri::State<'_, AppState>) -> KernelStatus {
    state.shared.status.lock().unwrap().clone()
}

/// splash 页“重试”按钮。
#[tauri::command]
pub fn dsh_retry(state: tauri::State<'_, AppState>) {
    let _ = state.tx.send(SuperCommand::Retry);
}
