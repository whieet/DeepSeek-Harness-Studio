// 内核（dsh web 子进程）生命周期：定位运行时、启动、就绪探测、日志、优雅停止与自动重启。
// 协议约定：内核 stdout 打印 "dsh web: http://127.0.0.1:<port>" 即视为就绪（与 CLI 行为一致）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, Url};
use tauri_plugin_opener::OpenerExt;

use crate::state::SuperCommand;

/// 启动超时：内核 90s 内未就绪视为启动失败。
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(90);
/// 就绪后崩溃的自动重启配额。
pub const MAX_AUTO_RESTARTS: u32 = 3;
/// 优雅停止宽限：TERM 后等待内核自行退出的时长。
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// stdout 就绪标记（与 dsh web 的 announceReady 输出一致）。
pub const URL_MARK: &str = "dsh web: http://127.0.0.1:";

// ── 状态与共享数据 ────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum KernelStatus {
    Starting { attempt: u32 },
    Restarting { attempt: u32 },
    Ready { url: String },
    Failed { error: String },
}

/// 监督线程与 UI 共享的运行时事实。
pub struct Shared {
    pub status: Mutex<KernelStatus>,
    pub port: Mutex<Option<u16>>,
    pub workspace: Mutex<PathBuf>,
    pub program: Mutex<PathBuf>,
    pub bundle: Mutex<PathBuf>,
    /// 运行时来源："dev" | "bundled" | "overlay"（诊断用）。
    pub source: Mutex<String>,
    pub node_version: Mutex<Option<String>>,
}

impl Shared {
    pub fn new(program: PathBuf, bundle: PathBuf, workspace: PathBuf) -> Self {
        Self {
            status: Mutex::new(KernelStatus::Starting { attempt: 0 }),
            port: Mutex::new(None),
            workspace: Mutex::new(workspace),
            program: Mutex::new(program),
            bundle: Mutex::new(bundle),
            source: Mutex::new(String::from("unknown")),
            node_version: Mutex::new(None),
        }
    }
}

fn set_status(shared: &Shared, status: KernelStatus) {
    *shared.status.lock().unwrap() = status;
}

// ── 运行时定位 ────────────────────────────────────────────────────────────────

pub struct KernelRuntime {
    pub program: PathBuf,
    pub bin_js: PathBuf,
    pub bundle: PathBuf,
    pub source: &'static str,
}

/// 覆盖层判定结果。
pub enum OverlayPick {
    /// 使用覆盖层运行时。
    Use { program: PathBuf, bin_js: PathBuf, bundle: PathBuf },
    /// 覆盖层存在但不值得使用（如版本回归），调用方应后台清理。
    IgnoreAndClean,
    /// 无有效覆盖层，忽略（保留现场便于诊断）。
    Ignore,
}

/// 读取 <dir>/package.json 的 version 字段。
pub fn read_pkg_version(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get("version")?.as_str().map(String::from)
}

/// 轻量 semver 比较（足够内核版本比较用）：逐段数值比较；预发布 < 正式发布；
/// 预发布标识逐段比较，数字段按数值且小于字母段。忽略前导 v；无法解析视为相等。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    fn parse(v: &str) -> Option<(Vec<u64>, Vec<String>)> {
        let v = v.trim();
        let v = v.strip_prefix('v').unwrap_or(v);
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (v, None),
        };
        let mut nums = Vec::new();
        for part in core.split('.') {
            nums.push(part.parse::<u64>().ok()?);
        }
        if nums.len() != 3 {
            return None;
        }
        let ids: Vec<String> = pre
            .map(|p| p.split('.').map(String::from).collect())
            .unwrap_or_default();
        Some((nums, ids))
    }

    fn cmp_ids(x: &[String], y: &[String]) -> Ordering {
        match (x.is_empty(), y.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        for (xi, yi) in x.iter().zip(y.iter()) {
            let ord = match (xi.parse::<u64>().ok(), yi.parse::<u64>().ok()) {
                (Some(na), Some(nb)) => na.cmp(&nb),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => xi.cmp(yi),
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        x.len().cmp(&y.len())
    }

    match (parse(a), parse(b)) {
        (Some((na, pa)), Some((nb, pb))) => na.cmp(&nb).then_with(|| cmp_ids(&pa, &pb)),
        _ => Ordering::Equal,
    }
}

/// 判定更新覆盖层是否可用（meta.json status=ready 且版本不回归于内置）。
pub fn pick_overlay(
    runtime_dir: &Path,
    bundled_version: Option<&str>,
    bundled_program: &Path,
    is_windows: bool,
) -> OverlayPick {
    let dsh_dir = runtime_dir.join("dsh");
    let bin_js = dsh_dir
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    if !bin_js.exists() {
        return OverlayPick::Ignore;
    }
    let Ok(raw) = std::fs::read_to_string(runtime_dir.join("meta.json")) else {
        return OverlayPick::Ignore;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return OverlayPick::Ignore;
    };
    if value.get("status").and_then(|v| v.as_str()) != Some("ready") {
        return OverlayPick::Ignore;
    }
    let Some(overlay_version) = value.get("dshVersion").and_then(|v| v.as_str()) else {
        return OverlayPick::Ignore;
    };
    // 版本回归保护：覆盖层不比内置新 → 忽略并清理（应用升级后自动用回新内置）。
    if let Some(bundled) = bundled_version {
        if compare_versions(overlay_version, bundled) != std::cmp::Ordering::Greater {
            return OverlayPick::IgnoreAndClean;
        }
    }
    let node_name = if is_windows { "dsh-node.exe" } else { "dsh-node" };
    let node_path = runtime_dir.join("node").join(node_name);
    let program = if node_path.exists() { node_path } else { bundled_program.to_path_buf() };
    OverlayPick::Use { program, bin_js, bundle: dsh_dir }
}

/// 定位内核运行时：
/// - 开发覆盖：DSH_APP_DIR 指向 dsh 包目录（可选 DSH_NODE 指定 node 可执行）；
/// - 发行形态：主程序同目录的 dsh-node 侧车 + 资源目录内的运行时闭包；
/// - 更新覆盖层：<app_data>/runtime（更新器落位的新版本，优先于内置）。
pub fn resolve_runtime(app: &AppHandle) -> Result<KernelRuntime, String> {
    if let Ok(app_dir) = std::env::var("DSH_APP_DIR") {
        let program = std::env::var("DSH_NODE").unwrap_or_else(|_| String::from("node"));
        return Ok(KernelRuntime {
            program: PathBuf::from(program),
            bin_js: PathBuf::from(&app_dir).join("lib").join("bin.js"),
            bundle: PathBuf::from(&app_dir),
            source: "dev",
        });
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or_else(|| String::from("无法定位可执行文件目录"))?;
    let sidecar_name = if cfg!(windows) { "dsh-node.exe" } else { "dsh-node" };
    let program = dir.join(sidecar_name);
    if !program.exists() {
        return Err(format!(
            "未找到内核运行时（{}）。请先运行打包脚本 stage-runtime.mjs。",
            program.display()
        ));
    }
    let bundle = app
        .path()
        .resource_dir()
        .map_err(|e| e.to_string())?
        .join("dsh");
    let bin_js = bundle
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    if !bin_js.exists() {
        return Err(format!("未找到内核入口（{}）。", bin_js.display()));
    }
    let bundled_version = read_pkg_version(&bundle.join("node_modules").join("@deepseek-ai").join("dsh"));
    if let Ok(data_dir) = app.path().app_data_dir() {
        let runtime_dir = data_dir.join("runtime");
        match pick_overlay(&runtime_dir, bundled_version.as_deref(), &program, cfg!(windows)) {
            OverlayPick::Use { program, bin_js, bundle } => {
                return Ok(KernelRuntime { program, bin_js, bundle, source: "overlay" });
            }
            OverlayPick::IgnoreAndClean => {
                log::info!("[dsh-desktop] 覆盖层内核不比内置新，后台清理：{}", runtime_dir.display());
                std::thread::spawn(move || {
                    let _ = std::fs::remove_dir_all(runtime_dir);
                });
            }
            OverlayPick::Ignore => {}
        }
    }
    Ok(KernelRuntime { program, bin_js, bundle, source: "bundled" })
}

// ── 版本信息（诊断用） ────────────────────────────────────────────────────────

pub fn node_version(shared: &Shared) -> String {
    let mut cached = shared.node_version.lock().unwrap();
    if let Some(value) = cached.as_ref() {
        return value.clone();
    }
    let program = shared.program.lock().unwrap().clone();
    let value = Command::new(&program)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| String::from("未知"));
    *cached = Some(value.clone());
    value
}

pub fn dsh_version(shared: &Shared) -> String {
    let bundle = shared.bundle.lock().unwrap().clone();
    let candidates = [
        bundle.join("node_modules").join("@deepseek-ai").join("dsh").join("package.json"),
        bundle.join("package.json"),
    ];
    for manifest in candidates {
        if let Ok(raw) = std::fs::read_to_string(&manifest) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(version) = value.get("version").and_then(|v| v.as_str()) {
                    return version.to_string();
                }
            }
        }
    }
    String::from("未知")
}

// ── 子进程管理 ────────────────────────────────────────────────────────────────

pub struct KernelRun {
    pub program: PathBuf,
    pub args: Vec<String>,
}

enum ChildEvent {
    Url(u16),
    Exit(Option<i32>),
}

pub struct KernelHandle {
    stop: Arc<AtomicBool>,
    receiver: Receiver<ChildEvent>,
}

/// 就绪行解析："dsh web: http://127.0.0.1:33456 (LAN: ...)" → Some(33456)。
pub fn parse_ready_line(line: &str) -> Option<u16> {
    let rest = line.trim().strip_prefix(URL_MARK)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u16>().ok()
}

/// 崩溃自动重启的退避间隔：1s / 2s / 4s，超出配额后封顶。
pub fn backoff_delay(attempt: u32) -> Duration {
    const STEPS: [u64; 3] = [1, 2, 4];
    let index = attempt.saturating_sub(1).clamp(0, (STEPS.len() - 1) as u32) as usize;
    Duration::from_secs(STEPS[index])
}

/// 内核日志：内核 stdout/stderr 原样落盘到 app 日志目录的 dsh.log。
pub struct LogSink {
    path: PathBuf,
    lock: Mutex<()>,
}

impl LogSink {
    pub fn new(path: PathBuf) -> Self {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Self { path, lock: Mutex::new(()) }
    }

    pub fn line(&self, text: &str) {
        let _guard = self.lock.lock();
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(file, "{}", text);
        }
    }
}

fn spawn_kernel(run: &KernelRun, cwd: &Path, log: &Arc<LogSink>) -> std::io::Result<KernelHandle> {
    let mut cmd = Command::new(&run.program);
    cmd.args(&run.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("stdout 已声明为管道");
    let stderr = child.stderr.take().expect("stderr 已声明为管道");
    let (tx, rx) = mpsc::channel::<ChildEvent>();
    let stop = Arc::new(AtomicBool::new(false));

    let tx1 = tx.clone();
    let log1 = Arc::clone(log);
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            log1.line(&line);
            if let Some(port) = parse_ready_line(&line) {
                let _ = tx1.send(ChildEvent::Url(port));
            }
        }
    });

    let log2 = Arc::clone(log);
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            log2.line(&line);
        }
    });

    let stop2 = Arc::clone(&stop);
    thread::spawn(move || watch_dog(child, stop2, tx));

    Ok(KernelHandle { stop, receiver: rx })
}

/// 看门狗：持有 Child，检测退出；收到 stop 标志后优雅终止（TERM → 宽限 → KILL / taskkill 树）。
fn watch_dog(mut child: Child, stop: Arc<AtomicBool>, tx: Sender<ChildEvent>) {
    loop {
        if stop.load(Ordering::SeqCst) {
            let pid = child.id().to_string();
            #[cfg(unix)]
            {
                let _ = Command::new("kill").arg("-TERM").arg(pid).status();
            }
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill").arg("/PID").arg(pid).arg("/T").status();
            }
            let deadline = Instant::now() + STOP_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let _ = tx.send(ChildEvent::Exit(status.code()));
                        return;
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            let _ = tx.send(ChildEvent::Exit(None));
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(ChildEvent::Exit(None));
                        return;
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = tx.send(ChildEvent::Exit(status.code()));
                return;
            }
            Ok(None) => thread::sleep(Duration::from_millis(150)),
            Err(_) => {
                let _ = tx.send(ChildEvent::Exit(None));
                return;
            }
        }
    }
}

fn stop_kernel(kernel: &mut Option<KernelHandle>) {
    if let Some(handle) = kernel.take() {
        handle.stop.store(true, Ordering::SeqCst);
        let _ = handle.receiver.recv_timeout(STOP_GRACE + Duration::from_secs(2));
    }
}

/// 就绪后 HTTP 探测：GET / 须返回 200。
pub fn http_is_ok(port: u16) -> bool {
    let addr: SocketAddr = match format!("127.0.0.1:{}", port).parse() {
        Ok(addr) => addr,
        Err(_) => return false,
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(800)) else {
        return false;
    };
    let crlf = String::from_utf8_lossy(&[13u8, 10u8][..]).into_owned();
    let request = format!("GET / HTTP/1.0{crlf}Host: 127.0.0.1{crlf}Connection: close{crlf}{crlf}");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut body = Vec::new();
    if stream.read_to_end(&mut body).is_err() {
        return false;
    }
    let head = String::from_utf8_lossy(&body);
    head.starts_with("HTTP/1.") && head.contains(" 200 ")
}

// ── 主窗口编排（监督线程通过 run_on_main_thread 调用） ────────────────────────

pub fn open_main_window(app: &AppHandle, url: &str) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|e| e.to_string())?;
    let port = parsed.port().ok_or_else(|| String::from("内核 URL 缺少端口"))?;
    if let Some(existing) = app.get_webview_window("main") {
        existing.navigate(parsed).map_err(|e| e.to_string())?;
        let _ = existing.show();
        let _ = existing.set_focus();
        return Ok(());
    }
    let opener_app = app.clone();
    tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(parsed))
        .title("DeepSeek Harness")
        .inner_size(1280.0, 800.0)
        .min_inner_size(800.0, 600.0)
        .center()
        .on_navigation(move |nav| {
            let allowed =
                nav.scheme() == "http" && nav.host_str() == Some("127.0.0.1") && nav.port() == Some(port);
            if !allowed {
                let _ = opener_app.opener().open_url(nav.as_str(), None::<&str>);
            }
            allowed
        })
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn close_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.destroy();
    }
}

// ── 监督线程 ─────────────────────────────────────────────────────────────────

enum WaitOutcome {
    Shutdown,
    Restart,
}

fn wait_command(rx: &Receiver<SuperCommand>) -> WaitOutcome {
    loop {
        match rx.recv() {
            Ok(SuperCommand::Shutdown) => return WaitOutcome::Shutdown,
            Ok(SuperCommand::Retry) | Ok(SuperCommand::RestartNow) => return WaitOutcome::Restart,
            Ok(SuperCommand::SetWorkspace(_)) => return WaitOutcome::Restart,
            Err(_) => return WaitOutcome::Shutdown,
        }
    }
}

/// 运行时解析器：每次内核（重）启动前调用，使更新覆盖层即时生效。
pub type RuntimeResolver = Arc<dyn Fn() -> Result<KernelRun, String> + Send + Sync>;

pub fn start_supervisor(
    app: AppHandle,
    shared: Arc<Shared>,
    resolver: RuntimeResolver,
    log_path: PathBuf,
) -> Sender<SuperCommand> {
    let (tx, rx) = mpsc::channel::<SuperCommand>();
    let log = Arc::new(LogSink::new(log_path));
    thread::spawn(move || run_supervisor(app, shared, resolver, rx, log));
    tx
}

fn run_supervisor(
    app: AppHandle,
    shared: Arc<Shared>,
    resolver: RuntimeResolver,
    rx: Receiver<SuperCommand>,
    log: Arc<LogSink>,
) {
    let mut kernel: Option<KernelHandle> = None;
    let mut auto_attempts: u32 = 0;
    let mut started_at = Instant::now();
    let mut saw_url = false;

    loop {
        // 1) 处理指令（保留最后一条）
        let mut command: Option<SuperCommand> = None;
        while let Ok(next) = rx.try_recv() {
            command = Some(next);
        }
        match command {
            Some(SuperCommand::Shutdown) => {
                log.line("[dsh-desktop] 收到退出指令，停止内核");
                stop_kernel(&mut kernel);
                app.exit(0);
                return;
            }
            Some(SuperCommand::RestartNow) | Some(SuperCommand::Retry) => {
                auto_attempts = 0;
                stop_kernel(&mut kernel);
                saw_url = false;
                continue;
            }
            Some(SuperCommand::SetWorkspace(dir)) => {
                *shared.workspace.lock().unwrap() = dir;
                auto_attempts = 0;
                stop_kernel(&mut kernel);
                saw_url = false;
                continue;
            }
            None => {}
        }

        // 2) 内核未运行则解析运行时并拉起（每次重新解析：覆盖层落位后重启即生效）
        if kernel.is_none() {
            let run = match resolver() {
                Ok(run) => run,
                Err(error) => {
                    log.line(&format!("[dsh-desktop] 运行时定位失败：{}", error));
                    set_status(
                        &shared,
                        KernelStatus::Failed { error: format!("运行时定位失败：{}", error) },
                    );
                    match wait_command(&rx) {
                        WaitOutcome::Shutdown => {
                            app.exit(0);
                            return;
                        }
                        WaitOutcome::Restart => {
                            auto_attempts = 0;
                            continue;
                        }
                    }
                }
            };
            let cwd = shared.workspace.lock().unwrap().clone();
            let status = if auto_attempts == 0 {
                KernelStatus::Starting { attempt: 0 }
            } else {
                KernelStatus::Restarting { attempt: auto_attempts }
            };
            set_status(&shared, status);
            log.line(&format!(
                "[dsh-desktop] 启动内核（cwd={}，自动重启配额已用 {}）",
                cwd.display(),
                auto_attempts
            ));
            match spawn_kernel(&run, &cwd, &log) {
                Ok(handle) => {
                    started_at = Instant::now();
                    saw_url = false;
                    kernel = Some(handle);
                }
                Err(error) => {
                    log.line(&format!("[dsh-desktop] 内核启动失败：{}", error));
                    set_status(
                        &shared,
                        KernelStatus::Failed { error: format!("内核启动失败：{}", error) },
                    );
                    match wait_command(&rx) {
                        WaitOutcome::Shutdown => {
                            app.exit(0);
                            return;
                        }
                        WaitOutcome::Restart => {
                            auto_attempts = 0;
                            continue;
                        }
                    }
                }
            }
            continue;
        }

        // 3) 等待内核事件
        let handle = kernel.as_mut().unwrap();
        match handle.receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(ChildEvent::Url(port)) => {
                if http_is_ok(port) {
                    saw_url = true;
                    let url = format!("http://127.0.0.1:{}/", port);
                    *shared.port.lock().unwrap() = Some(port);
                    set_status(&shared, KernelStatus::Ready { url: url.clone() });
                    auto_attempts = 0;
                    log.line(&format!("[dsh-desktop] 内核就绪：{}", url));
                    let app2 = app.clone();
                    let url2 = url;
                    let _ = app.run_on_main_thread(move || {
                        if let Err(error) = open_main_window(&app2, &url2) {
                            log::error!("打开主窗口失败：{}", error);
                        }
                    });
                }
            }
            Ok(ChildEvent::Exit(code)) => {
                kernel = None;
                saw_url = false;
                log.line(&format!("[dsh-desktop] 内核退出（code={:?}）", code));
                let app2 = app.clone();
                let _ = app.run_on_main_thread(move || close_main_window(&app2));
                if auto_attempts < MAX_AUTO_RESTARTS {
                    auto_attempts += 1;
                    let delay = backoff_delay(auto_attempts);
                    set_status(&shared, KernelStatus::Restarting { attempt: auto_attempts });
                    log.line(&format!(
                        "[dsh-desktop] {}ms 后自动重启（第 {} 次）",
                        delay.as_millis(),
                        auto_attempts
                    ));
                    thread::sleep(delay);
                } else {
                    let code_text = code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| String::from("无"));
                    set_status(
                        &shared,
                        KernelStatus::Failed {
                            error: format!(
                                "内核已退出（code={}），自动重启 {} 次未恢复。",
                                code_text, MAX_AUTO_RESTARTS
                            ),
                        },
                    );
                    match wait_command(&rx) {
                        WaitOutcome::Shutdown => {
                            app.exit(0);
                            return;
                        }
                        WaitOutcome::Restart => {
                            auto_attempts = 0;
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !saw_url && started_at.elapsed() > BOOT_TIMEOUT {
                    log.line("[dsh-desktop] 启动超时（90s 未就绪）");
                    stop_kernel(&mut kernel);
                    if auto_attempts < MAX_AUTO_RESTARTS {
                        auto_attempts += 1;
                        let delay = backoff_delay(auto_attempts);
                        set_status(&shared, KernelStatus::Restarting { attempt: auto_attempts });
                        thread::sleep(delay);
                    } else {
                        set_status(
                            &shared,
                            KernelStatus::Failed { error: String::from("内核启动超时（90s 未就绪）。") },
                        );
                        match wait_command(&rx) {
                            WaitOutcome::Shutdown => {
                                app.exit(0);
                                return;
                            }
                            WaitOutcome::Restart => {
                                auto_attempts = 0;
                            }
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                kernel = None;
            }
        }
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ready_line_extracts_port() {
        assert_eq!(parse_ready_line("dsh web: http://127.0.0.1:3080"), Some(3080));
        assert_eq!(
            parse_ready_line("dsh web: http://127.0.0.1:33456 (LAN: http://192.168.1.4:33456)"),
            Some(33456)
        );
        assert_eq!(parse_ready_line("random log"), None);
        assert_eq!(parse_ready_line("dsh web: http://127.0.0.1:"), None);
    }

    #[test]
    fn backoff_delay_grows_and_caps() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(99), Duration::from_secs(4));
    }

    #[test]
    fn http_is_ok_rejects_dead_port() {
        assert!(!http_is_ok(1));
    }

    #[test]
    fn compare_versions_orders_prereleases() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("0.1.1-rc.2", "0.1.1"), Ordering::Less);
        assert_eq!(compare_versions("0.1.1", "0.1.1-rc.2"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.1-rc.2", "0.1.1-rc.10"), Ordering::Less);
        assert_eq!(compare_versions("0.2.0", "0.1.9"), Ordering::Greater);
        assert_eq!(compare_versions("v1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0-alpha", "1.0.0-alpha.1"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0-alpha.1", "1.0.0-beta"), Ordering::Less);
        assert_eq!(compare_versions("abc", "1.0.0"), Ordering::Equal);
    }

    fn make_overlay(dir: &Path, version: &str, status: &str) {
        std::fs::create_dir_all(
            dir.join("dsh")
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib"),
        )
        .unwrap();
        std::fs::write(
            dir.join("dsh")
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js"),
            "",
        )
        .unwrap();
        std::fs::write(
            dir.join("meta.json"),
            format!("{{ \"status\": \"{}\", \"dshVersion\": \"{}\" }}", status, version),
        )
        .unwrap();
    }

    #[test]
    fn pick_overlay_prefers_newer_ready_overlay() {
        let dir = std::env::temp_dir().join(format!("dsh-ov-test-{}-a", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        make_overlay(&dir, "9.9.9", "ready");
        let bundled = dir.join("bundled-node");
        match pick_overlay(&dir, Some("0.1.1"), &bundled, false) {
            OverlayPick::Use { program, bin_js, bundle } => {
                assert_eq!(program, bundled);
                assert!(bin_js.ends_with("bin.js"));
                assert!(bundle.ends_with("dsh"));
            }
            _ => panic!("应使用覆盖层"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pick_overlay_flags_version_regression() {
        let dir = std::env::temp_dir().join(format!("dsh-ov-test-{}-b", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        make_overlay(&dir, "0.0.1", "ready");
        match pick_overlay(&dir, Some("0.1.1"), &dir.join("node"), false) {
            OverlayPick::IgnoreAndClean => {}
            _ => panic!("版本回归应触发清理"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pick_overlay_ignores_failed_or_missing() {
        let dir = std::env::temp_dir().join(format!("dsh-ov-test-{}-c", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 无 meta
        assert!(matches!(pick_overlay(&dir, None, &dir.join("n"), false), OverlayPick::Ignore));
        // status != ready
        make_overlay(&dir, "9.9.9", "failed");
        assert!(matches!(pick_overlay(&dir, None, &dir.join("n"), false), OverlayPick::Ignore));
        // meta 缺 dshVersion
        std::fs::write(dir.join("meta.json"), "{ \"status\": \"ready\" }").unwrap();
        assert!(matches!(pick_overlay(&dir, None, &dir.join("n"), false), OverlayPick::Ignore));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fake_kernel_becomes_ready_then_stops() {
        let tmp = std::env::temp_dir().join(format!("dsh-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let log = Arc::new(LogSink::new(tmp.join("test.log")));
        let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("fake-sidecar.mjs");
        let run = KernelRun {
            program: PathBuf::from("node"),
            args: vec![fake.to_string_lossy().into_owned(), String::from("--port"), String::from("0")],
        };
        let handle = spawn_kernel(&run, &tmp, &log).expect("spawn fake kernel");
        let mut port = None;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            match handle.receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(ChildEvent::Url(found)) => {
                    port = Some(found);
                    break;
                }
                Ok(ChildEvent::Exit(code)) => panic!("fake kernel exited early: {:?}", code),
                Err(_) => {}
            }
        }
        let port = port.expect("应收到就绪 URL 事件");
        assert!(http_is_ok(port), "GET / 应返回 200");
        handle.stop.store(true, Ordering::SeqCst);
        let mut stopped = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match handle.receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(ChildEvent::Exit(_)) => {
                    stopped = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(stopped, "内核应在宽限期内退出");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
