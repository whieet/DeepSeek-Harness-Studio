// 更新编排：内核（npm registry）与客户端（GitHub Releases）两通道版本检查、
// 内核运行时更新应用（update.mjs 子进程，随包 Node 侧车执行）、进度状态与
// updater 窗口。客户端通道一期仅"检查 + 打开下载页"；签名落地后二期接入
// tauri-plugin-updater 原地升级，入口与本模块对外接口保持不变。
//
// update.mjs 协议：
//   check → stdout 单行 JSON { kernel, app }
//   apply → stdout JSON 行 { event: "progress" | "error" | "done", ... }

use serde::Serialize;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

use crate::config;
use crate::server::{self, KernelStatus};
use crate::state::{AppState, SuperCommand};

/// 内核就绪后到首次自动检查的延迟。
const AUTO_CHECK_DELAY: Duration = Duration::from_secs(3);
/// 等待内核就绪的上限（超时则本次跳过自动检查）。
const READY_WAIT: Duration = Duration::from_secs(150);

// ── 状态 ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelInfo {
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    /// update_available 且未被"此版本跳过"——面板是否渲染该行。
    pub prompt: bool,
    pub url: Option<String>,
    pub engines_node: Option<String>,
    pub node_sufficient: Option<bool>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum UpdatePhase {
    Idle,
    Checking,
    Applying { phase: String, pct: u32, message: String },
    PendingRestart { dsh_version: String },
    Failed { error: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub phase: UpdatePhase,
    pub kernel: Option<ChannelInfo>,
    pub app: Option<ChannelInfo>,
    /// 本次结果来自手动"检查更新"（面板显示"已是最新"而不是不渲染）。
    pub manual: bool,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self { phase: UpdatePhase::Idle, kernel: None, app: None, manual: false }
    }
}

#[derive(Default)]
pub struct UpdateState(pub Mutex<UpdateStatus>);

fn with_status<R>(app: &AppHandle, f: impl FnOnce(&mut UpdateStatus) -> R) -> R {
    let state = app.state::<UpdateState>();
    let mut guard = state.0.lock().unwrap();
    f(&mut guard)
}

fn set_failed(app: &AppHandle, error: String) {
    log::error!("[update] {}", error);
    with_status(app, |st| st.phase = UpdatePhase::Failed { error });
}

fn compute_prompt(info: &mut Option<ChannelInfo>, skipped: Option<&str>) {
    if let Some(info) = info.as_mut() {
        info.prompt = info.update_available && info.latest.as_deref() != skipped;
    }
}

fn runtime_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join("runtime"))
}

fn updater_script(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(
        app
            .path()
            .resource_dir()
            .map_err(|e| e.to_string())?
            .join("update")
            .join("update.mjs")
    )
}

struct ProcOutput {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

fn spawn_capture(program: &std::path::Path, args: &[String]) -> Result<ProcOutput, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // Node ≥24 支持 NODE_USE_ENV_PROXY：企业代理环境常用。
    if std::env::var_os("HTTPS_PROXY").is_some() || std::env::var_os("https_proxy").is_some() {
        cmd.env("NODE_USE_ENV_PROXY", "1");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("无法启动更新器（{}）：{}", program.display(), e))?;
    Ok(ProcOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
    })
}

fn parse_last_json(text: &str) -> Result<serde_json::Value, String> {
    text.lines()
        .rev()
        .find_map(|line| {
            let t = line.trim();
            if t.starts_with('{') {
                serde_json::from_str::<serde_json::Value>(t).ok()
            } else {
                None
            }
        })
        .ok_or_else(|| String::from("更新器未输出有效 JSON 结果"))
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(String::from)
}

fn parse_channel(value: &serde_json::Value, current_fallback: &str) -> ChannelInfo {
    ChannelInfo {
        current: str_field(value, "current").unwrap_or_else(|| current_fallback.to_string()),
        latest: value.get("latest").and_then(|v| v.as_str()).map(String::from),
        update_available: value.get("updateAvailable").and_then(|v| v.as_bool()).unwrap_or(false),
        prompt: false, // 由 compute_prompt 填充
        url: value.get("url").and_then(|v| v.as_str()).map(String::from),
        engines_node: value.get("enginesNode").and_then(|v| v.as_str()).map(String::from),
        node_sufficient: value.get("nodeSufficient").and_then(|v| v.as_bool()),
        reason: value.get("reason").and_then(|v| v.as_str()).map(String::from),
    }
}

// ── 检查 ─────────────────────────────────────────────────────────────────────

fn run_check(app: &AppHandle) -> Result<(Option<ChannelInfo>, Option<ChannelInfo>), String> {
    let (program, kernel_current) = {
        let state = app.state::<AppState>();
        let program = state.shared.program.lock().unwrap().clone();
        let version = server::dsh_version(&state.shared);
        (program, version)
    };
    let script = updater_script(app)?;
    let app_current = env!("CARGO_PKG_VERSION").to_string();

    let output = spawn_capture(
        &program,
        &[
            script.to_string_lossy().into_owned(),
            String::from("check"),
            String::from("--kernel-current"),
            kernel_current.clone(),
            String::from("--app-current"),
            app_current.clone(),
        ],
    )?;

    let value = parse_last_json(&output.stdout).map_err(|e| {
        let tail: String = output.stderr.chars().rev().take(300).collect();
        let tail: String = tail.chars().rev().collect();
        format!("{}（exit={:?}，stderr 尾部：{}）", e, output.code, tail)
    })?;
    let kernel = value.get("kernel").map(|v| parse_channel(v, &kernel_current));
    let app = value.get("app").map(|v| parse_channel(v, &app_current));
    Ok((kernel, app))
}

/// 执行两通道检查。manual=true 来自菜单"检查更新…"（完成后总是打开面板）。
pub fn check_and_prompt(app: &AppHandle, manual: bool) {
    {
        let state = app.state::<UpdateState>();
        let mut guard = state.0.lock().unwrap();
        if matches!(guard.phase, UpdatePhase::Checking | UpdatePhase::Applying { .. }) {
            if manual {
                open_on_main(app);
            }
            return;
        }
        guard.phase = UpdatePhase::Checking;
        guard.manual = manual;
    }
    let app2 = app.clone();
    std::thread::spawn(move || match run_check(&app2) {
        Ok((mut kernel, mut app_channel)) => {
            let settings = config::load(&app2);
            compute_prompt(&mut kernel, settings.skipped_kernel_version.as_deref());
            compute_prompt(&mut app_channel, settings.skipped_app_version.as_deref());
            let should_open = with_status(&app2, |st| {
                st.kernel = kernel;
                st.app = app_channel;
                st.phase = UpdatePhase::Idle;
                st.manual
                    || st.kernel.as_ref().is_some_and(|c| c.prompt)
                    || st.app.as_ref().is_some_and(|c| c.prompt)
            });
            if should_open {
                open_on_main(&app2);
            }
        }
        Err(error) => {
            let manual = with_status(&app2, |st| {
                st.phase = UpdatePhase::Failed { error };
                st.manual
            });
            if manual {
                open_on_main(&app2);
            }
        }
    });
}

/// 启动自动检查线程：等内核就绪 → 延迟 3s → 检查（autoCheck 关闭则跳过）。
pub fn start_auto_check(app: &AppHandle) {
    let app2 = app.clone();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + READY_WAIT;
        loop {
            let ready = {
                let state = app2.state::<AppState>();
                let is_ready = matches!(*state.shared.status.lock().unwrap(), KernelStatus::Ready { .. });
                is_ready
            };
            if ready || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        std::thread::sleep(AUTO_CHECK_DELAY);
        if config::auto_check_enabled(&app2) {
            check_and_prompt(&app2, false);
        }
    });
}

// ── 应用更新（仅内核通道） ───────────────────────────────────────────────────

enum ApplyEvent {
    Error(String),
    Done(String),
}

pub fn begin_apply(app: &AppHandle) {
    let latest = {
        let state = app.state::<UpdateState>();
        let guard = state.0.lock().unwrap();
        match &guard.phase {
            UpdatePhase::Checking | UpdatePhase::Applying { .. } => return,
            _ => {}
        }
        match guard.kernel.as_ref().and_then(|k| k.latest.clone()) {
            Some(latest) => latest,
            None => return,
        }
    };
    with_status(app, |st| {
        st.phase = UpdatePhase::Applying {
            phase: String::from("prepare"),
            pct: 0,
            message: String::from("准备更新…"),
        };
    });
    let app2 = app.clone();
    std::thread::spawn(move || run_apply(&app2, latest));
}

fn run_apply(app: &AppHandle, version: String) {
    let program = {
        let state = app.state::<AppState>();
        let program = state.shared.program.lock().unwrap().clone();
        program
    };
    let script = match updater_script(app) {
        Ok(p) => p,
        Err(e) => {
            set_failed(app, e);
            return;
        }
    };
    let npm_cli = match app.path().resource_dir() {
        Ok(d) => d.join("npm").join("bin").join("npm-cli.js"),
        Err(e) => {
            set_failed(app, e.to_string());
            return;
        }
    };
    let dest = match runtime_dir(app) {
        Ok(d) => d,
        Err(e) => {
            set_failed(app, e);
            return;
        }
    };
    let cache = dest
        .parent()
        .map(|p| p.join("update-cache"))
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&cache);

    let mut cmd = Command::new(&program);
    cmd.args([
        script.to_string_lossy().into_owned(),
        String::from("apply"),
        String::from("--version"),
        version.clone(),
        String::from("--dest"),
        dest.to_string_lossy().into_owned(),
        String::from("--npm"),
        npm_cli.to_string_lossy().into_owned(),
        String::from("--cache"),
        cache.to_string_lossy().into_owned(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if std::env::var_os("HTTPS_PROXY").is_some() || std::env::var_os("https_proxy").is_some() {
        cmd.env("NODE_USE_ENV_PROXY", "1");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            set_failed(app, format!("无法启动更新器子进程：{}", e));
            return;
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        set_failed(app, String::from("更新器 stdout 缺失"));
        return;
    };
    let stderr = child.stderr.take();

    let (evt_tx, evt_rx) = mpsc::channel::<ApplyEvent>();
    let reader_app = app.clone();
    let reader = std::thread::spawn(move || {
        let lines = BufReader::new(stdout).lines();
        for line in lines.map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            match value.get("event").and_then(|v| v.as_str()) {
                Some("progress") => {
                    let phase = str_field(&value, "phase").unwrap_or_default();
                    let pct = value.get("pct").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let message = str_field(&value, "message").unwrap_or_default();
                    with_status(&reader_app, |st| {
                        st.phase = UpdatePhase::Applying { phase, pct, message };
                    });
                }
                Some("error") => {
                    let _ = evt_tx.send(ApplyEvent::Error(
                        str_field(&value, "message").unwrap_or_else(|| String::from("更新失败")),
                    ));
                }
                Some("done") => {
                    let _ = evt_tx.send(ApplyEvent::Done(
                        str_field(&value, "dshVersion").unwrap_or_default(),
                    ));
                }
                _ => {}
            }
        }
    });

    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let tail_clone = Arc::clone(&stderr_tail);
    let stderr_thread = stderr.map(|pipe| {
        let tail = Arc::clone(&tail_clone);
        std::thread::spawn(move || {
            let lines = BufReader::new(pipe).lines();
            for line in lines.map_while(Result::ok) {
                let mut guard = tail.lock().unwrap();
                guard.push_str(&line);
                guard.push('\n');
                let len = guard.len();
                if len > 2000 {
                    guard.drain(0..len - 2000);
                }
            }
        })
    });

    let wait_status = child.wait();
    let _ = reader.join();
    if let Some(handle) = stderr_thread {
        let _ = handle.join();
    }

    let mut final_error: Option<String> = None;
    let mut done_version: Option<String> = None;
    while let Ok(event) = evt_rx.try_recv() {
        match event {
            ApplyEvent::Error(msg) => final_error = Some(msg),
            ApplyEvent::Done(v) => done_version = Some(v),
        }
    }

    let ok = wait_status.as_ref().map(|s| s.success()).unwrap_or(false);
    if ok && final_error.is_none() {
        let applied = done_version.unwrap_or_else(|| version.clone());
        with_status(app, |st| {
            st.phase = UpdatePhase::PendingRestart { dsh_version: applied };
        });
        log::info!("[update] 内核运行时更新完成：{}（待重启生效）", version);
    } else {
        let tail = stderr_tail.lock().unwrap().chars().rev().take(400).collect::<String>();
        let tail: String = tail.chars().rev().collect();
        let error = final_error.unwrap_or_else(|| {
            format!("更新器异常退出（code={:?}）{}",
                wait_status.ok().and_then(|s| s.code()),
                if tail.is_empty() { String::new() } else { format!("：{}", tail) })
        });
        set_failed(app, error);
    }
}

// ── updater 窗口 ─────────────────────────────────────────────────────────────

pub fn open_updater_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("updater") {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "updater", WebviewUrl::App("update.html".into()))
        .title("软件更新")
        .inner_size(520.0, 520.0)
        .min_inner_size(440.0, 420.0)
        .center()
        .build();
}

fn close_updater_window(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(window) = app2.get_webview_window("updater") {
            let _ = window.close();
        }
    });
}

fn open_on_main(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || open_updater_window(&app2));
}

// ── 菜单辅助 ─────────────────────────────────────────────────────────────────

/// 删除更新覆盖层并重启内核（恢复应用内置运行时）。
pub fn restore_bundled_kernel(app: &AppHandle) {
    match runtime_dir(app) {
        Ok(dir) => {
            if dir.exists() {
                if let Err(error) = std::fs::remove_dir_all(&dir) {
                    log::error!("[update] 删除覆盖层失败：{}", error);
                }
            }
            let state = app.state::<AppState>();
            let _ = state.tx.send(SuperCommand::RestartNow);
        }
        Err(error) => log::error!("[update] 无法定位覆盖层目录：{}", error),
    }
}

/// 覆盖层状态摘要（诊断用）。
pub fn overlay_summary(app: &AppHandle) -> String {
    match runtime_dir(app) {
        Ok(dir) => {
            if let Ok(raw) = std::fs::read_to_string(dir.join("meta.json")) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                    let version = value.get("dshVersion").and_then(|v| v.as_str()).unwrap_or("?");
                    let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                    return format!("覆盖层 dsh {}（{}）", version, status);
                }
            }
            String::from("无覆盖层（使用内置运行时）")
        }
        Err(_) => String::from("覆盖层不可用"),
    }
}

/// 最近一次检查结果摘要（诊断用）。
pub fn latest_summary(app: &AppHandle) -> String {
    let state = app.state::<UpdateState>();
    let guard = state.0.lock().unwrap();
    let kernel = guard
        .kernel
        .as_ref()
        .and_then(|c| c.latest.clone())
        .unwrap_or_else(|| String::from("-"));
    let app_latest = guard
        .app
        .as_ref()
        .and_then(|c| c.latest.clone())
        .unwrap_or_else(|| String::from("-"));
    format!("内核最新 {} / 客户端最新 {}", kernel, app_latest)
}

// ── Tauri 命令 ───────────────────────────────────────────────────────────────

#[tauri::command]
pub fn update_state(state: State<UpdateState>) -> UpdateStatus {
    state.0.lock().unwrap().clone()
}

#[tauri::command]
pub fn update_begin(app: AppHandle) {
    begin_apply(&app);
}

#[tauri::command]
pub fn update_open_download(app: AppHandle) {
    let url = with_status(&app, |st| st.app.as_ref().and_then(|a| a.url.clone()));
    if let Some(url) = url {
        let _ = app.opener().open_url(url, None::<&str>);
    }
}

#[tauri::command]
pub fn update_skip(app: AppHandle, channel: String) {
    let latest = with_status(&app, |st| {
        match channel.as_str() {
            "app" => st.app.as_ref().and_then(|a| a.latest.clone()),
            _ => st.kernel.as_ref().and_then(|k| k.latest.clone()),
        }
    });
    let Some(latest) = latest else { return };

    let mut settings = config::load(&app);
    match channel.as_str() {
        "app" => settings.skipped_app_version = Some(latest),
        _ => settings.skipped_kernel_version = Some(latest),
    }
    if let Err(error) = config::save(&app, &settings) {
        log::error!("[update] 保存跳过版本设置失败：{}", error);
    }

    let (skipped_kernel, skipped_app) = (settings.skipped_kernel_version, settings.skipped_app_version);
    let any_prompt = with_status(&app, |st| {
        compute_prompt(&mut st.kernel, skipped_kernel.as_deref());
        compute_prompt(&mut st.app, skipped_app.as_deref());
        st.kernel.as_ref().is_some_and(|c| c.prompt) || st.app.as_ref().is_some_and(|c| c.prompt)
    });
    if !any_prompt {
        close_updater_window(&app);
    }
}

#[tauri::command]
pub fn update_dismiss(app: AppHandle) {
    close_updater_window(&app);
}

#[tauri::command]
pub fn update_restart_kernel(app: AppHandle) {
    let state = app.state::<AppState>();
    let _ = state.tx.send(SuperCommand::RestartNow);
    close_updater_window(&app);
}

#[tauri::command]
pub fn update_check_now(app: AppHandle) {
    check_and_prompt(&app, true);
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_last_json_finds_last_object_line() {
        let text = concat!("log line\n", "{\"a\":1}\n", "{\"a\":2,\"b\":\"x\"}\n");
        let value = parse_last_json(text).unwrap();
        assert_eq!(value.get("a").and_then(|v| v.as_i64()), Some(2));
        assert!(parse_last_json("no json here").is_err());
    }

    #[test]
    fn parse_channel_maps_fields() {
        let value = serde_json::json!({
            "current": "0.1.1-rc.2",
            "latest": "0.1.2",
            "updateAvailable": true,
            "enginesNode": ">=22",
            "nodeSufficient": true,
        });
        let info = parse_channel(&value, "fallback");
        assert_eq!(info.current, "0.1.1-rc.2");
        assert_eq!(info.latest.as_deref(), Some("0.1.2"));
        assert!(info.update_available);
        assert!(!info.prompt);
        assert_eq!(info.engines_node.as_deref(), Some(">=22"));
        assert_eq!(info.node_sufficient, Some(true));
    }

    #[test]
    fn compute_prompt_respects_skipped_versions() {
        let mut info = Some(ChannelInfo {
            current: "0.1.0".into(),
            latest: Some("0.2.0".into()),
            update_available: true,
            prompt: false,
            url: None,
            engines_node: None,
            node_sufficient: None,
            reason: None,
        });
        compute_prompt(&mut info, None);
        assert!(info.as_ref().unwrap().prompt);
        compute_prompt(&mut info, Some("0.2.0"));
        assert!(!info.as_ref().unwrap().prompt);
        compute_prompt(&mut info, Some("0.1.9"));
        assert!(info.as_ref().unwrap().prompt);
    }
}
