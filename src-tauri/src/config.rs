// 应用配置：工作目录等。存放在系统约定的 app 配置目录（macOS: ~/Library/Application Support/...）。
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default)]
    pub workspace_root: Option<String>,
    /// 启动时自动检查更新（默认开启；None 视为开启）。
    #[serde(default)]
    pub auto_check: Option<bool>,
    /// “此版本跳过”的内核版本。
    #[serde(default)]
    pub skipped_kernel_version: Option<String>,
    /// “此版本跳过”的客户端版本。
    #[serde(default)]
    pub skipped_app_version: Option<String>,
}

/// 是否在启动时自动检查更新（菜单手动检查不受此开关影响）。
pub fn auto_check_enabled(app: &AppHandle) -> bool {
    load(app).auto_check != Some(false)
}

pub fn settings_file(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("settings.json")
}

pub fn load(app: &AppHandle) -> AppSettings {
    let path = settings_file(app);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_file(app);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let raw = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(path, raw).map_err(|e| e.to_string())
}

/// 默认工作目录：用户主目录（无 dirs 依赖，直接读环境变量）。
pub fn home_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(dir) = std::env::var("USERPROFILE") {
            return PathBuf::from(dir);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(dir) = std::env::var("HOME") {
            return PathBuf::from(dir);
        }
    }
    PathBuf::from(".")
}

/// 当前生效的工作目录（设置优先，否则主目录）。
pub fn current_workspace(app: &AppHandle) -> PathBuf {
    let settings = load(app);
    settings
        .workspace_root
        .map(PathBuf::from)
        .unwrap_or_else(home_dir)
}
