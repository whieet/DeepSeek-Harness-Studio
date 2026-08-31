// 右侧工作区侧边栏：文件树 / 全目录搜索 / Git 面板的命令层。
// 安全面：所有文件路径 canonicalize 后必须位于当前工作区内，拒绝逃逸。
// Git 通过系统 git CLI（cwd = 工作区），未安装 git 时返回友好错误。

use serde::Serialize;
use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::server;
use crate::state::AppState;

/// 侧栏面板是否收起（竖向图标条常驻）。
static SIDEBAR_HIDDEN: AtomicBool = AtomicBool::new(false);
/// 侧栏面板宽度（f64 以 bits 存 AtomicU64），初始 340.0。
static SIDEBAR_PANEL_W: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(340.0_f64.to_bits());

/// 内核会话工作区（dsh-sidebar-bridge 插件上报的最新会话 cwd）。
static BRIDGE_CWD: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn sidebar_hidden() -> bool {
    SIDEBAR_HIDDEN.load(Ordering::SeqCst)
}

pub fn sidebar_panel_w() -> f64 {
    f64::from_bits(SIDEBAR_PANEL_W.load(Ordering::SeqCst))
}

fn workspace_root(app: &AppHandle) -> Result<PathBuf, String> {
    // 优先跟随内核会话工作区（插件桥上报）；不可用时回退到应用工作区。
    if let Ok(guard) = BRIDGE_CWD.lock() {
        if let Some(p) = guard.as_ref() {
            if p.is_dir() {
                return Ok(p.clone());
            }
        }
    }
    let state = app.state::<AppState>();
    let root = state.shared.workspace.lock().unwrap().clone();
    if !root.is_dir() {
        return Err(format!("工作区目录不存在：{}", root.display()));
    }
    Ok(root)
}

// ── 内核会话工作区桥（dsh-sidebar-bridge 插件）───────────────────────────────

/// 读取插件写下的桥端口（~/.dsh/sidebar-bridge.port）。
fn bridge_port() -> Option<u16> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".dsh").join("sidebar-bridge.port");
    let meta = std::fs::metadata(&path).ok()?;
    // 过期防护：端口文件超过 10 分钟未刷新视为失效。
    if let Ok(age) = meta.modified().map(|m| m.elapsed().map(|e| e.as_secs()).unwrap_or(0)) {
        if age > 600 {
            return None;
        }
    }
    let text = std::fs::read_to_string(&path).ok()?;
    text.trim().parse::<u16>().ok()
}

/// 桥 /state 响应（只取需要的字段）。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeState {
    #[serde(default)]
    current_cwd: Option<String>,
}

fn bridge_fetch(port: u16) -> Option<BridgeState> {
    let mut stream = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(800),
    )
    .ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(800)))
        .ok()?;
    use std::io::Write;
    let _ = write!(
        stream,
        "GET /state HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    );
    let mut raw = String::new();
    let _ = std::io::Read::by_ref(&mut stream).take(4 * 1024 * 1024).read_to_string(&mut raw);
    let sep = raw.find("\r\n\r\n")?;
    let head = &raw[..sep];
    // 状态行必须 200。
    let status_ok = head.split_whitespace().nth(1).is_some_and(|c| c == "200");
    if !status_ok {
        return None;
    }
    let body = &raw[sep + 4..];
    let state: BridgeState = serde_json::from_str(body.trim()).ok()?;
    Some(state)
}

/// 启动轮询线程：每 2 秒读取桥状态；会话工作区变化时通知侧栏刷新。
pub fn start_bridge_poll(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last: Option<PathBuf> = None;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let cwd = bridge_port()
                .and_then(bridge_fetch)
                .and_then(|s| s.current_cwd)
                .map(PathBuf::from)
                .filter(|p| p.is_dir());
            let Some(cwd) = cwd else { continue };
            let changed = last.as_ref().map(|p| p != &cwd).unwrap_or(true);
            if changed {
                if let Ok(mut guard) = BRIDGE_CWD.lock() {
                    *guard = Some(cwd.clone());
                }
                let _ = app.emit("dsh-sidebar-workspace", cwd.display().to_string());
                last = Some(cwd);
            }
        }
    });
}

/// 启动前确保桥插件已安装进 web profile（拷贝插件 + 登记 patch 层，均幂等）。
pub fn ensure_bridge_installed(app: &AppHandle) {
    let Some(resource) = app.path().resource_dir().ok().map(|d| d.join("dsh-sidebar-bridge")) else {
        return;
    };
    if !resource.join("index.js").exists() {
        return;
    }
    let Some(home) = std::env::var("HOME").ok().map(PathBuf::from) else {
        return;
    };
    let dst = home
        .join(".dsh")
        .join("profiles")
        .join("web")
        .join("node_modules")
        .join("dsh-sidebar-bridge");
    let same = std::fs::read_to_string(dst.join("index.js"))
        .map(|old| old == std::fs::read_to_string(resource.join("index.js")).unwrap_or_default())
        .unwrap_or(false);
    if !same {
        let _ = std::fs::remove_dir_all(&dst);
        let _ = std::fs::create_dir_all(&dst);
        for name in ["index.js", "package.json"] {
            let _ = std::fs::copy(resource.join(name), dst.join(name));
        }
    }
    let patch_path = home.join(".dsh").join("profiles").join("web").join("cordis.patch.yml");
    let patch = std::fs::read_to_string(&patch_path).unwrap_or_default();
    if !patch.contains("dsh-sidebar-bridge") {
        let entry = "\n# dsh-desktop 侧边栏数据桥（桌面壳轮询会话/工作区；升级 dsh 不受影响）\n\
- insert:\n    - id: dsh-sidebar-bridge\n      name: dsh-sidebar-bridge\n";
        let _ = std::fs::write(&patch_path, patch.trim_end().to_string() + entry);
    }
}

/// 校验 path 位于 root 内（防符号链接/相对路径逃逸），返回 canonical 路径。
fn ensure_inside(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let root_canon = root.canonicalize().map_err(|e| e.to_string())?;
    let path_canon = path
        .canonicalize()
        .map_err(|e| format!("路径无效（{}）：{}", path.display(), e))?;
    if path_canon.starts_with(&root_canon) {
        Ok(path_canon)
    } else {
        Err(format!("路径超出工作区范围：{}", path.display()))
    }
}

/// rename/mkdir 等操作的目标可能尚不存在：校验其父目录在工作区内。
fn ensure_parent_inside(root: &Path, path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| String::from("路径缺少父目录"))?;
    ensure_inside(root, parent).map(|_| ())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub path: String,
    pub name: String,
    pub is_git_repo: bool,
    pub git_missing: bool,
}

#[tauri::command]
pub fn workspace_info(app: AppHandle) -> Result<WorkspaceInfo, String> {
    let root = workspace_root(&app)?;
    let is_git_repo = root.join(".git").exists();
    let git_missing = is_git_repo && !git_available();
    Ok(WorkspaceInfo {
        path: root.display().to_string(),
        name: root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string()),
        is_git_repo,
        git_missing,
    })
}

// ── 文件系统 ─────────────────────────────────────────────────────────────────

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// 忽略的目录名（树与搜索共用）。
fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "out"
            | "coverage"
            | "vendor"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".next"
            | ".cache"
            | "log"
            | "logs"
            | ".gradle"
            | ".idea"
    )
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<std::fs::DirEntry>, String> {
    let mut entries: Vec<std::fs::DirEntry> = dir
        .read_dir()
        .map_err(|e| format!("{}：{}", dir.display(), e))?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by(|a, b| {
        let ad = a.path().is_dir();
        let bd = b.path().is_dir();
        bd.cmp(&ad).then_with(|| {
            a.file_name()
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.file_name().to_string_lossy().to_lowercase())
        })
    });
    Ok(entries)
}

/// 列出某目录一层内容（懒加载树）。dir 传空串 = 工作区根。
#[tauri::command]
pub fn fs_list(app: AppHandle, dir: String) -> Result<serde_json::Value, String> {
    let root = workspace_root(&app)?;
    let target = if dir.is_empty() {
        root.canonicalize().map_err(|e| e.to_string())?
    } else {
        ensure_inside(&root, Path::new(&dir))?
    };
    if !target.is_dir() {
        return Err(format!("不是目录：{}", target.display()));
    }
    let entries = read_dir_sorted(&target)?;
    let items: Vec<serde_json::Value> = entries
        .iter()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            !(e.path().is_dir() && is_ignored_dir(&name))
        })
        .map(|e| {
            serde_json::json!({
                "name": e.file_name().to_string_lossy(),
                "path": e.path().display().to_string(),
                "kind": if e.path().is_dir() { "dir" } else { "file" },
            })
        })
        .collect();
    Ok(serde_json::json!({ "path": target.display().to_string(), "items": items }))
}

const TEXT_LIMIT: u64 = 1_500_000;
const IMAGE_LIMIT: u64 = 8_000_000;

fn image_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_string_lossy().to_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "ico" => Some("image/x-icon"),
        _ => None,
    }
}

/// 简单二进制嗅探：头部含 NUL 即视为二进制。
fn looks_binary(buf: &[u8]) -> bool {
    buf.iter().take(8192).any(|&b| b == 0)
}

#[tauri::command]
pub fn fs_read(app: AppHandle, path: String) -> Result<serde_json::Value, String> {
    let root = workspace_root(&app)?;
    let target = ensure_inside(&root, Path::new(&path))?;
    let meta = std::fs::metadata(&target).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err(format!("是目录而非文件：{}", target.display()));
    }
    let size = meta.len();
    if let Some(mime) = image_mime(&target) {
        if size > IMAGE_LIMIT {
            return Ok(serde_json::json!({ "kind": "tooLarge", "size": size }));
        }
        let buf = std::fs::read(&target).map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({
            "kind": "image",
            "mime": mime,
            "data": base64_encode(&buf),
        }));
    }
    if size > TEXT_LIMIT {
        return Ok(serde_json::json!({ "kind": "tooLarge", "size": size }));
    }
    let mut file = std::fs::File::open(&target).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if looks_binary(&buf) {
        return Ok(serde_json::json!({ "kind": "binary", "size": size }));
    }
    Ok(serde_json::json!({
        "kind": "text",
        "content": String::from_utf8_lossy(&buf).into_owned(),
        "size": size,
    }))
}

/// 极简 base64（无第三方依赖）。
fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(TBL[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TBL[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn require_dir(app: &AppHandle, dir: &str) -> Result<PathBuf, String> {
    let root = workspace_root(app)?;
    let target = if dir.is_empty() {
        root.canonicalize().map_err(|e| e.to_string())?
    } else {
        ensure_inside(&root, Path::new(dir))?
    };
    if !target.is_dir() {
        return Err(format!("不是目录：{}", target.display()));
    }
    Ok(target)
}

#[tauri::command]
pub fn fs_new_file(app: AppHandle, dir: String, name: String) -> Result<(), String> {
    let target = require_dir(&app, &dir)?;
    let path = target.join(&name);
    ensure_parent_inside(&workspace_root(&app)?, &path)?;
    if path.exists() {
        return Err(format!("已存在：{}", name));
    }
    std::fs::File::create(&path).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn fs_mkdir(app: AppHandle, dir: String, name: String) -> Result<(), String> {
    let target = require_dir(&app, &dir)?;
    let path = target.join(&name);
    ensure_parent_inside(&workspace_root(&app)?, &path)?;
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn fs_rename(app: AppHandle, path: String, new_name: String) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let target = ensure_inside(&root, Path::new(&path))?;
    let dest = target
        .parent()
        .ok_or_else(|| String::from("路径缺少父目录"))?
        .join(&new_name);
    ensure_parent_inside(&root, &dest)?;
    std::fs::rename(&target, &dest).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn fs_delete(app: AppHandle, path: String) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let target = ensure_inside(&root, Path::new(&path))?;
    if target.is_dir() {
        std::fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
    } else {
        std::fs::remove_file(&target).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn fs_copy_path(path: String) -> Result<(), String> {
    match arboard::Clipboard::new().and_then(|mut c| c.set_text(path)) {
        Ok(()) => Ok(()),
        Err(e) => Err(format!("复制失败：{}", e)),
    }
}

#[tauri::command]
pub fn fs_reveal(app: AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let root = workspace_root(&app)?;
    let target = ensure_inside(&root, Path::new(&path))?;
    app.opener()
        .reveal_item_in_dir(target)
        .map_err(|e| e.to_string())
}

// ── 搜索 ─────────────────────────────────────────────────────────────────────

const SEARCH_MAX_FILES: usize = 2000;
const SEARCH_MAX_MATCHES: usize = 1000;
const SEARCH_FILE_SIZE_LIMIT: u64 = 1_000_000;
const SEARCH_MAX_PER_FILE: usize = 50;

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub preview: String,
}

/// 分支信息。
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GitBranch {
    pub name: String,
    pub kind: String,
    pub current: bool,
    pub upstream: String,
    pub time: i64,
    pub author: String,
    pub short: String,
    pub subject: String,
}

/// 提交信息。
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitInfo {
    pub hash: String,
    pub short: String,
    pub author: String,
    pub time: i64,
    pub subject: String,
    pub refs: String,
}

/// 提交内文件。
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitFile {
    pub status: String,
    pub path: String,
}

/// 文件名/目录名命中（相对路径 + 种类）。
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PathHit {
    pub path: String,
    pub kind: String, // "file" | "dir"
}

/// 搜索总输出：内容命中 + 路径命中。
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SearchOutcome {
    pub matches: Vec<SearchMatch>,
    pub paths: Vec<PathHit>,
    pub truncated: bool,
}

/// 广度优先收集可搜索条目（文件与目录）：浅层先扫，总量截断。
/// 目录同时入队遍历并记入 out（is_dir=true），供路径搜索；文件需满足大小限制才计入。
fn walk_entries(root: &Path, out: &mut Vec<(PathBuf, bool)>, stop: &AtomicBool) {
    let cap = SEARCH_MAX_FILES * 5;
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        if out.len() >= cap || stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if !is_ignored_dir(&name) && !name.starts_with('.') {
                    if out.len() < cap {
                        out.push((path.clone(), true));
                    }
                    queue.push_back(path);
                }
            } else if path.is_file() {
                let small = std::fs::metadata(&path).map(|m| m.len() <= SEARCH_FILE_SIZE_LIMIT).unwrap_or(false);
                if small {
                    if out.len() >= cap {
                        return;
                    }
                    out.push((path, false));
                }
            }
        }
    }
}

/// 兼容包装：只要文件列表（内容扫描用）。
fn walk_files(root: &Path, out: &mut Vec<PathBuf>, stop: &AtomicBool) {
    let mut entries = Vec::new();
    walk_entries(root, &mut entries, stop);
    out.extend(entries.into_iter().filter(|(_, is_dir)| !is_dir).map(|(p, _)| p));
}

#[tauri::command]
pub async fn search_workspace(
    app: AppHandle,
    query: String,
    case_sensitive: bool,
    whole_word: bool,
) -> Result<SearchOutcome, String> {
    let root = workspace_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        search_in_dir(&root, &query, case_sensitive, whole_word)
    })
    .await
    .map_err(|e| format!("搜索任务失败：{}", e))?
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// whole_word 时检查 needle 在 hay 中是否至少有一处词边界命中。
fn has_word_boundary_match(hay: &str, needle: &str) -> bool {
    let mut offset = 0usize;
    while let Some(pos) = hay[offset..].find(needle) {
        let abs = offset + pos;
        let before_ok = abs == 0 || !is_word_char(hay[..abs].chars().next_back().unwrap());
        let after_ok = abs + needle.len() >= hay.len() || !is_word_char(hay[abs + needle.len()..].chars().next().unwrap());
        if before_ok && after_ok {
            return true;
        }
        offset = abs + 1;
        while !hay.is_char_boundary(offset.min(hay.len())) { offset += 1; }
        if offset > hay.len() { break; }
    }
    false
}

/// 路径命中上限（文件名/目录名匹配）。
const SEARCH_MAX_PATHS: usize = 300;

/// 核心扫描逻辑（供命令与测试共用）：
/// 路径匹配（文件名/目录名，相对路径子串）+ 内容匹配（逐文件子串）。
fn search_in_dir(root: &Path, query: &str, case_sensitive: bool, whole_word: bool) -> Result<SearchOutcome, String> {
    if query.is_empty() {
        return Ok(SearchOutcome { matches: Vec::new(), paths: Vec::new(), truncated: false });
    }
    let needle = if case_sensitive { query.to_string() } else { query.to_lowercase() };
    let stop = AtomicBool::new(false);
    let mut entries: Vec<(PathBuf, bool)> = Vec::new();
    walk_entries(root, &mut entries, &stop);

    // 路径命中：文件名与目录名（整条相对路径子串匹配，天然覆盖各级目录段）。
    let mut paths: Vec<PathHit> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut path_truncated = false;
    for (path, is_dir) in &entries {
        let rel = path.strip_prefix(root).unwrap_or(path).display().to_string();
        let hay = if case_sensitive { rel.clone() } else { rel.to_lowercase() };
        if hay.contains(&needle) && (!whole_word || has_word_boundary_match(&hay, &needle)) {
            if paths.len() >= SEARCH_MAX_PATHS {
                path_truncated = true;
                break;
            }
            paths.push(PathHit {
                path: rel,
                kind: if *is_dir { "dir".into() } else { "file".into() },
            });
        }
        if !*is_dir {
            files.push(path.clone());
        }
    }

    let mut matches: Vec<SearchMatch> = Vec::new();
    let mut files_scanned = 0usize;
    let mut content_truncated = false;
    'outer: for file in &files {
        if matches.len() >= SEARCH_MAX_MATCHES {
            content_truncated = true;
            break;
        }
        let Ok(mut content) = std::fs::read(file) else { continue };
        if looks_binary(&content) {
            continue;
        }
        files_scanned += 1;
        if files_scanned > SEARCH_MAX_FILES {
            content_truncated = true;
            break;
        }
        let text = String::from_utf8_lossy(&content).into_owned();
        content.clear();
        // 预览取原文；匹配用按需大小写折叠的副本。
        let lower = if case_sensitive { None } else { Some(text.to_lowercase()) };
        let hay = lower.as_deref().unwrap_or(&text);
        let rel = file
            .strip_prefix(root)
            .unwrap_or(file)
            .display()
            .to_string();
        let mut per_file = 0usize;
        let mut offset = 0usize;
        while let Some(pos) = hay[offset..].find(&needle) {
            let abs = offset + pos;
            if whole_word {
                let before_ok = abs == 0 || !is_word_char(hay[..abs].chars().next_back().unwrap());
                let after_ok = abs + needle.len() >= hay.len() || !is_word_char(hay[abs + needle.len()..].chars().next().unwrap());
                if !before_ok || !after_ok {
                    offset = abs + 1;
                    // 字节边界：跳到下一个 UTF-8 字符边界
                    while !hay.is_char_boundary(offset.min(hay.len())) { offset += 1; }
                    if offset >= hay.len() { break; }
                    continue;
                }
            }
            let line_no = hay[..abs].matches(0x0A as char).count() + 1;
            let line_start = hay[..abs].rfind(0x0A as char).map(|i| i + 1).unwrap_or(0);
            let line_end = hay[abs..].find(0x0A as char).map(|i| abs + i).unwrap_or(hay.len());
            let preview: String = text[line_start..line_end].chars().take(200).collect();
            matches.push(SearchMatch {
                file: rel.clone(),
                line: line_no,
                col: abs - line_start,
                preview,
            });
            per_file += 1;
            if per_file >= SEARCH_MAX_PER_FILE || matches.len() >= SEARCH_MAX_MATCHES {
                content_truncated = true;
                continue 'outer;
            }
            offset = abs + needle.len();
        }
    }
    Ok(SearchOutcome { matches, paths, truncated: path_truncated || content_truncated })
}

#[cfg(test)]
mod tests {
    fn search_in_dir_w(root: &Path, q: &str, cs: bool) -> Result<SearchOutcome, String> {
        search_in_dir(root, q, cs, false)
    }
    use super::*;

    #[test]
    fn search_finds_and_previews_original_case() {
        let dir = std::env::temp_dir().join(format!("dsh-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "Hello Bridge\nsecond line\n").unwrap();
        std::fs::write(dir.join("sub").join("b.md"), "nothing here\nBRIDGE inside\n").unwrap();

        // 大小写不敏感：两文件都命中，预览保留原文大小写
        let out = search_in_dir_w(&dir, "bridge", false).unwrap();
        assert_eq!(out.matches.len(), 2, "应命中两处：{:?}", out.matches);
        assert!(out.matches[0].preview.starts_with("Hello Bridge"), "预览应为原文：{}", out.matches[0].preview);
        assert_eq!(out.matches[0].line, 1);
        assert!(out.matches[1].preview.starts_with("BRIDGE inside"), "预览应为原文：{}", out.matches[1].preview);
        assert_eq!(out.matches[1].file.replace('\\', "/"), "sub/b.md");

        // 路径命中：a.txt（文件名）与 sub（目录名）……"bridge" 不在文件名里 → 路径命中应为空
        assert!(out.paths.is_empty(), "bridge 不在路径中：{:?}", out.paths);

        // 大小写敏感：只命中原文大写 BRIDGE
        let cs = search_in_dir_w(&dir, "BRIDGE", true).unwrap();
        assert_eq!(cs.matches.len(), 1);
        assert_eq!(cs.matches[0].line, 2);

        // 文件名搜索：命中 a.txt；目录名搜索：命中 sub/
        let byname = search_in_dir_w(&dir, "a.txt", false).unwrap();
        assert_eq!(byname.paths.len(), 1);
        assert_eq!(byname.paths[0].path, "a.txt");
        assert_eq!(byname.paths[0].kind, "file");
        let bydir = search_in_dir_w(&dir, "sub", false).unwrap();
        assert!(bydir.paths.iter().any(|h| h.path == "sub" && h.kind == "dir"), "{:?}", bydir.paths);

        // 空查询
        let empty = search_in_dir_w(&dir, "", false).unwrap();
        assert!(empty.matches.is_empty() && empty.paths.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }


    #[test]
    fn git_branches_and_remote_checkout() {
        let dir = std::env::temp_dir().join(format!("dsh-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| git(&dir, args).expect("git 失败");
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        // 伪造远程跟踪分支（免建裸仓库）
        run(&["update-ref", "refs/remotes/origin/main", "HEAD"]);

        let bs = git_branches_in(&dir).unwrap();
        let local = bs.iter().find(|b| b.name == "main" && b.kind == "local");
        let remote = bs.iter().find(|b| b.name == "origin/main" && b.kind == "remote");
        assert!(local.is_some(), "{:?}", bs);
        assert!(remote.is_some(), "{:?}", bs);
        assert!(local.unwrap().current);

        // 切到远程分支：应创建同名本地跟踪分支，而不是 detached HEAD
        git_checkout_in(&dir, "origin/main", false).unwrap();
        let cur = git(&dir, &["branch", "--show-current"]).unwrap();
        assert_eq!(cur.trim(), "main", "应落在本地 main 而非 detached HEAD");
        let st = git(&dir, &["status", "--porcelain", "--branch"]).unwrap();
        assert!(st.starts_with("## main"), "不应出现 HEAD (no branch): {}", st.lines().next().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_whole_word_filters_substrings() {
        let dir = std::env::temp_dir().join(format!("dsh-word-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "let bridge_x = 1;\nlet bridge = 2;\nlet Bridge = 3;\n").unwrap();
        std::fs::write(dir.join("data.txt"), "x\n").unwrap();

        // 子串模式：3 处都命中（bridge_x 含 bridge 子串）
        let sub = search_in_dir(&dir, "bridge", false, false).unwrap();
        assert_eq!(sub.matches.len(), 3, "{:?}", sub.matches);

        // 整词模式：只命中独立词 bridge / Bridge（大小写不敏感）
        let ww = search_in_dir(&dir, "bridge", false, true).unwrap();
        assert_eq!(ww.matches.len(), 2, "{:?}", ww.matches);
        assert_eq!(ww.matches[0].line, 2);
        assert_eq!(ww.matches[1].line, 3);

        // 路径整词：a.rs 开头的 a 是整词；data.txt 中的 a 前面是 d，非整词
        let path_sub = search_in_dir(&dir, "a", false, false).unwrap();
        assert!(path_sub.paths.iter().any(|p| p.path == "a.rs"));
        assert!(path_sub.paths.iter().any(|p| p.path == "data.txt"));
        let path_ww = search_in_dir(&dir, "a", false, true).unwrap();
        assert!(path_ww.paths.iter().any(|p| p.path == "a.rs"), "{:?}", path_ww.paths);
        assert!(!path_ww.paths.iter().any(|p| p.path == "data.txt"), "data 中的 a 非整词：{:?}", path_ww.paths);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_real_project_finds_bridge() {
        // 真实仓库冒烟：工作区=本仓库源码目录，"sidebar-bridge" 必有命中（插件目录与引用）。
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let out = search_in_dir_w(&root, "sidebar-bridge", false).unwrap();
        assert!(!out.matches.is_empty(), "真实仓库应命中 sidebar-bridge");
        assert!(
            out.matches.iter().any(|h| h.file.contains("dsh-sidebar-bridge") || h.file.contains("sidebar.rs") || h.file.contains("main.rs")),
            "命中应来自插件/壳源码：{:?}",
            out.matches.iter().take(5).map(|h| &h.file).collect::<Vec<_>>()
        );
        // 路径命中：dsh-sidebar-bridge 目录本身
        assert!(
            out.paths.iter().any(|p| p.kind == "dir" && p.path.contains("sidebar-bridge")),
            "目录名应命中：{:?}",
            out.paths.iter().take(5).map(|p| &p.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn walk_is_breadth_first_and_skips_ignored() {
        let dir = std::env::temp_dir().join(format!("dsh-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("deep").join("deeper")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("root.txt"), "x").unwrap();
        std::fs::write(dir.join("deep").join("mid.txt"), "x").unwrap();
        std::fs::write(dir.join("deep").join("deeper").join("low.txt"), "x").unwrap();
        std::fs::write(dir.join("node_modules").join("pkg.js"), "x").unwrap();

        let stop = AtomicBool::new(false);
        let mut out = Vec::new();
        walk_files(&dir, &mut out, &stop);
        let names: Vec<String> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // BFS：浅层先于深层；node_modules 被忽略
        assert_eq!(names, vec!["root.txt", "mid.txt", "low.txt"], "顺序应为浅层优先：{:?}", names);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── Git ──────────────────────────────────────────────────────────────────────

/// 在工作区执行 git，返回 stdout；失败时错误带 stderr（截断）。
fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                String::from("未检测到 git 命令。请安装 Git（macOS: xcode-select --install / Windows: Git for Windows）后重试。")
            } else {
                format!("执行 git 失败：{}", e)
            }
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let stderr = if stderr.len() > 600 {
            format!("{}…", stderr.chars().rev().take(600).collect::<String>().chars().rev().collect::<String>())
        } else {
            stderr.to_string()
        };
        return Err(if stderr.is_empty() {
            format!("git {} 失败（exit {:?}）", args.join(" "), output.status.code())
        } else {
            stderr
        });
    }
    Ok(stdout)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitChange {
    pub path: String,
    pub status: String,
    pub staged: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatus {
    pub branch: String,
    pub ahead: usize,
    pub behind: usize,
    pub staged: Vec<GitChange>,
    pub unstaged: Vec<GitChange>,
}

/// porcelain v1 -z：XY<space>path\0。X=暂存区，Y=工作区。
#[tauri::command]
pub fn git_status(app: AppHandle) -> Result<GitStatus, String> {
    let root = workspace_root(&app)?;
    if !root.join(".git").exists() {
        return Err(String::from("当前目录不是 git 仓库"));
    }
    let raw = git(&root, &["status", "--porcelain", "-z", "--branch"])?;
    let mut branch = String::from("HEAD");
    let mut ahead = 0usize;
    let mut behind = 0usize;
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    for rec in raw.split(0u8 as char) {
        if rec.is_empty() {
            continue;
        }
        if let Some(rest) = rec.strip_prefix("## ") {
            let head_line = rest.split('\0').next().unwrap_or(rest);
            branch = head_line
                .split("...")
                .next()
                .unwrap_or(head_line)
                .trim_start_matches("No commits yet on ")
                .to_string();
            let marks = rest.split('\0').skip(1).collect::<Vec<_>>().join(" ");
            for part in marks.split(|c: char| c == ',' || c == '[') {
                let p = part.trim();
                if let Some(n) = p.strip_prefix("ahead ") {
                    ahead = n.trim_end_matches(']').parse().unwrap_or(0);
                }
                if let Some(n) = p.strip_prefix("behind ") {
                    behind = n.trim_end_matches(']').parse().unwrap_or(0);
                }
            }
            continue;
        }
        if rec.len() < 4 {
            continue;
        }
        let x = rec.as_bytes()[0] as char;
        let y = rec.as_bytes()[1] as char;
        let file = rec[3..].trim_matches('"').to_string();
        if x != '?' && x != ' ' {
            staged.push(GitChange { path: file.clone(), status: x.to_string(), staged: true });
        }
        if y != ' ' || x == '?' {
            let s = if x == '?' { "?".to_string() } else { y.to_string() };
            unstaged.push(GitChange { path: file, status: s, staged: false });
        }
    }
    Ok(GitStatus { branch, ahead, behind, staged, unstaged })
}

#[tauri::command]
pub fn git_add(app: AppHandle, paths: Vec<String>) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let mut args: Vec<&str> = vec!["add", "--"];
    args.extend(paths.iter().map(|s| s.as_str()));
    git(&root, &args).map(|_| ())
}

#[tauri::command]
pub fn git_unstage(app: AppHandle, paths: Vec<String>) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let mut args: Vec<&str> = vec!["reset", "HEAD", "--"];
    args.extend(paths.iter().map(|s| s.as_str()));
    git(&root, &args).map(|_| ())
}

#[tauri::command]
pub fn git_discard(app: AppHandle, path: String) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let target = ensure_inside(&root, root.join(&path).as_path())?;
    let raw = git(&root, &["status", "--porcelain", "-z", "--", &path])?;
    let untracked = raw.split(0u8 as char).any(|rec| rec.starts_with("??"));
    if untracked {
        if target.is_dir() {
            std::fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
        } else {
            std::fs::remove_file(&target).map_err(|e| e.to_string())?;
        }
    } else {
        git(&root, &["checkout", "--", &path]).map(|_| ())?;
    }
    Ok(())
}

#[tauri::command]
pub fn git_commit(app: AppHandle, message: String) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let message = message.trim().to_string();
    if message.is_empty() {
        return Err(String::from("提交信息不能为空"));
    }
    git(&root, &["commit", "-m", &message]).map(|_| ())
}

#[tauri::command]
pub fn git_push(app: AppHandle) -> Result<(), String> {
    let root = workspace_root(&app)?;
    git(&root, &["push"]).map(|_| ())
}

#[tauri::command]
pub fn git_pull(app: AppHandle) -> Result<(), String> {
    let root = workspace_root(&app)?;
    git(&root, &["pull"]).map(|_| ())
}

#[tauri::command]
pub fn git_diff_file(app: AppHandle, path: String, staged: bool) -> Result<String, String> {
    let root = workspace_root(&app)?;
    let mut args = vec!["diff", "--no-color"];
    if staged {
        args.push("--cached");
    }
    args.push("--");
    args.push(&path);
    git(&root, &args)
}


/// 初始化 git 仓库（非仓库目录时）。
#[tauri::command]
pub fn git_init(app: AppHandle) -> Result<(), String> {
    let root = workspace_root(&app)?;
    if root.join(".git").exists() {
        return Ok(());
    }
    git(&root, &["init"]).map(|_| ())
}

/// 分支列表（本地 + 远程），当前分支置首标记。
#[tauri::command]
pub fn git_branches(app: AppHandle) -> Result<Vec<GitBranch>, String> {
    let root = workspace_root(&app)?;
    git_branches_in(&root)
}

fn git_branches_in(root: &Path) -> Result<Vec<GitBranch>, String> {
    if !root.join(".git").exists() {
        return Err(String::from("当前目录不是 git 仓库"));
    }
    let fmt = "%(refname)%00%(HEAD)%00%(upstream:short)%00%(committerdate:unix)%00%(authorname)%00%(objectname:short)%00%(contents:subject)";
    let raw = git(root, &["for-each-ref", &format!("--format={}", fmt), "refs/heads", "refs/remotes"])?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches(' ');
        if line.is_empty() { continue; }
        let mut parts = line.split(' ');
        let refname = parts.next().unwrap_or("").trim().to_string();
        let head = parts.next().unwrap_or("").trim() == "*";
        let upstream = parts.next().unwrap_or("").trim().to_string();
        let time = parts.next().unwrap_or("0").trim().parse::<i64>().unwrap_or(0);
        let author = parts.next().unwrap_or("").trim().to_string();
        let short = parts.next().unwrap_or("").trim().to_string();
        let subject = parts.next().unwrap_or("").trim().to_string();
        let (kind, display) = if let Some(n) = refname.strip_prefix("refs/heads/") {
            ("local", n.to_string())
        } else if let Some(n) = refname.strip_prefix("refs/remotes/") {
            ("remote", n.to_string())
        } else {
            continue;
        };
        if display.ends_with("/HEAD") { continue; }
        out.push(GitBranch { name: display, kind: kind.to_string(), current: head, upstream, time, author, short, subject });
    }
    // 当前分支置首，其余按最后提交时间倒序（VSCode 同款：最近活动的在前）
    out.sort_by(|a, b| {
        b.current
            .cmp(&a.current)
            .then(b.time.cmp(&a.time))
    });
    Ok(out)
}

/// 切换分支；new_branch 为 true 时先创建。
/// 目标为远程分支（origin/xxx）时：本地有同名分支直接切，否则创建跟踪分支——避免 detached HEAD。
#[tauri::command]
pub fn git_checkout(app: AppHandle, branch: String, new_branch: bool) -> Result<(), String> {
    let root = workspace_root(&app)?;
    git_checkout_in(&root, &branch, new_branch)
}

fn git_checkout_in(root: &Path, branch: &str, new_branch: bool) -> Result<(), String> {
    if !root.join(".git").exists() {
        return Err(String::from("当前目录不是 git 仓库"));
    }
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(String::from("分支名不能为空"));
    }
    if new_branch {
        git(root, &["checkout", "-b", branch]).map(|_| ())
    } else if let Some(local) = branch.strip_prefix("origin/") {
        if local.is_empty() || local == "HEAD" {
            return Err(format!("无法切换到 {}", branch));
        }
        let full = format!("refs/heads/{}", local);
        let exists = git(root, &["show-ref", "--verify", "--quiet", &full]).is_ok();
        if exists {
            git(root, &["checkout", local]).map(|_| ())
        } else {
            git(root, &["checkout", "-b", local, "--track", branch]).map(|_| ())
        }
    } else {
        git(root, &["checkout", branch]).map(|_| ())
    }
}

/// 提交历史（最近 max 条）。
#[tauri::command]
pub fn git_log(app: AppHandle, max: Option<usize>) -> Result<Vec<GitCommitInfo>, String> {
    let root = workspace_root(&app)?;
    if !root.join(".git").exists() {
        return Err(String::from("当前目录不是 git 仓库"));
    }
    let max = max.unwrap_or(50).clamp(1, 500);
    let max_s = max.to_string();
    let sep: char = '';
    let fmt_str: String = ["%H", "%h", "%an", "%at", "%s", "%D"].join(&sep.to_string());
    let fmt_arg = format!("--pretty=format:{}", fmt_str);
    let max_arg = format!("--max-count={}", max_s);
    let raw = git(&root, &["log", &fmt_arg, &max_arg])?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.split(sep).collect();
        if parts.len() < 5 { continue; }
        let refs = parts.get(5).unwrap_or(&"").to_string();
        out.push(GitCommitInfo {
            hash: parts[0].to_string(),
            short: parts[1].to_string(),
            author: parts[2].to_string(),
            time: parts[3].parse::<i64>().unwrap_or(0),
            subject: parts[4].to_string(),
            refs,
        });
    }
    Ok(out)
}

/// 某提交的改动文件列表（show --name-status）。
#[tauri::command]
pub fn git_commit_files(app: AppHandle, hash: String) -> Result<Vec<GitCommitFile>, String> {
    let root = workspace_root(&app)?;
    let raw = git(&root, &["show", "--name-status", "--format=", &hash])?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let mut parts = line.split('\t');
        let status = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        if path.is_empty() { continue; }
        out.push(GitCommitFile { status, path });
    }
    Ok(out)
}

/// 某提交的完整 diff（给查看器）。
#[tauri::command]
pub fn git_commit_diff(app: AppHandle, hash: String) -> Result<String, String> {
    let root = workspace_root(&app)?;
    git(&root, &["show", "--no-color", "--format=", &hash])
}

/// 撤销某提交（revert，生成反向提交，安全）。
#[tauri::command]
pub fn git_revert(app: AppHandle, hash: String) -> Result<(), String> {
    let root = workspace_root(&app)?;
    git(&root, &["revert", "--no-edit", &hash]).map(|_| ())
}

/// 抓取远程更新（不改工作区）。
#[tauri::command]
pub fn git_fetch(app: AppHandle) -> Result<(), String> {
    let root = workspace_root(&app)?;
    if !root.join(".git").exists() {
        return Err(String::from("当前目录不是 git 仓库"));
    }
    git(&root, &["fetch", "--all", "--prune"]).map(|_| ())
}

/// 贮藏列表。
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StashEntry {
    pub index: usize,
    pub message: String,
    pub time: i64,
}

#[tauri::command]
pub fn git_stash_list(app: AppHandle) -> Result<Vec<StashEntry>, String> {
    let root = workspace_root(&app)?;
    if !root.join(".git").exists() {
        return Err(String::from("当前目录不是 git 仓库"));
    }
    let raw = git(&root, &["stash", "list", "--format=%gd%x00%at%x00%gs"])?;
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let mut parts = line.split(' ');
        let _ref = parts.next().unwrap_or("");
        let time = parts.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
        let message = parts.next().unwrap_or("").trim().to_string();
        out.push(StashEntry { index: i, message, time });
    }
    Ok(out)
}

/// 贮藏当前更改（含未跟踪）。
#[tauri::command]
pub fn git_stash_push(app: AppHandle, message: Option<String>) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let has_changes = !git(&root, &["status", "--porcelain"])?.trim().is_empty();
    if !has_changes {
        return Err(String::from("没有可贮藏的更改"));
    }
    match message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => git(&root, &["stash", "push", "-u", "-m", m]).map(|_| ()),
        None => git(&root, &["stash", "push", "-u"]).map(|_| ()),
    }
}

/// 恢复最近一条贮藏（可能有冲突，错误原样返回）。
#[tauri::command]
pub fn git_stash_pop(app: AppHandle) -> Result<(), String> {
    let root = workspace_root(&app)?;
    git(&root, &["stash", "pop"]).map(|_| ())
}

/// 修正上次提交：把当前暂存并入，沿用/替换提交信息。
#[tauri::command]
pub fn git_commit_amend(app: AppHandle, message: Option<String>) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let has_head = git(&root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok();
    if !has_head {
        return Err(String::from("还没有可修正的提交"));
    }
    let msg = message.as_deref().map(str::trim).filter(|m| !m.is_empty());
    match msg {
        Some(m) => git(&root, &["commit", "--amend", "-m", m]).map(|_| ()),
        None => git(&root, &["commit", "--amend", "--no-edit"]).map(|_| ()),
    }
}

/// 撤销上次提交（更改保留在暂存区，soft reset）。
#[tauri::command]
pub fn git_undo_commit(app: AppHandle) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let has_head = git(&root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok();
    if !has_head {
        return Err(String::from("还没有可撤销的提交"));
    }
    git(&root, &["reset", "--soft", "HEAD~1"]).map(|_| ())
}

/// 删除分支（-d 安全删除；force 时 -D）。
#[tauri::command]
pub fn git_branch_delete(app: AppHandle, branch: String, force: bool) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(String::from("分支名不能为空"));
    }
    let mut args = vec!["branch", if force { "-D" } else { "-d" }];
    args.push(branch);
    git(&root, &args).map(|_| ())
}

/// 合并指定分支到当前分支。
#[tauri::command]
pub fn git_merge(app: AppHandle, branch: String) -> Result<(), String> {
    let root = workspace_root(&app)?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(String::from("分支名不能为空"));
    }
    git(&root, &["merge", branch]).map(|_| ())
}

// ── 侧栏窗口控制与工作区事件 ─────────────────────────────────────────────────

/// 调整侧栏面板宽度并重排布局。
#[tauri::command]
pub fn set_sidebar_width(app: AppHandle, width: f64) -> Result<(), String> {
    let width = width.clamp(260.0, 600.0);
    SIDEBAR_PANEL_W.store(width.to_bits(), Ordering::SeqCst);
    relayout(&app);
    Ok(())
}

/// 折叠/展开侧栏面板（竖向图标条常驻）。
#[tauri::command]
pub fn toggle_sidebar(app: AppHandle) -> Result<bool, String> {
    let next = !SIDEBAR_HIDDEN.load(Ordering::SeqCst);
    SIDEBAR_HIDDEN.store(next, Ordering::SeqCst);
    relayout(&app);
    Ok(next)
}

fn relayout(app: &AppHandle) {
    if let Some(window) = app.get_window("main") {
        server::layout_main_window(&window);
    }
}

/// 工作区变更：通知侧栏刷新（supervisor 线程调用）。
pub fn emit_workspace_changed(app: &AppHandle, path: &Path) {
    let _ = app.emit("dsh-sidebar-workspace", path.display().to_string());
}

#[tauri::command]
pub fn open_viewer(
    app: AppHandle,
    path: String,
    line: usize,
    mode: String,
    hash: Option<String>,
) -> Result<(), String> {
    let mut q = format!("mode={}", percent_encode(&mode));
    if !path.is_empty() {
        q.push_str(&format!("&path={}", percent_encode(&path)));
    }
    if line > 0 {
        q.push_str(&format!("&line={}", line));
    }
    if let Some(h) = hash.as_deref().filter(|h| !h.is_empty()) {
        q.push_str(&format!("&hash={}", percent_encode(h)));
    }
    let script = format!("location.replace('viewer.html?{}')", q);
    if let Some(existing) = app.get_webview_window("viewer") {
        existing.eval(&script).map_err(|e| e.to_string())?;
        let _ = existing.show();
        let _ = existing.set_focus();
        return Ok(());
    }
    let window = tauri::WebviewWindowBuilder::new(
        &app,
        "viewer",
        tauri::WebviewUrl::App("viewer.html".into()),
    )
    .title("查看文件")
    .inner_size(900.0, 700.0)
    .min_inner_size(520.0, 380.0)
    .resizable(true)
    .center()
    .build()
    .map_err(|e| e.to_string())?;
    window.eval(&script).map_err(|e| e.to_string())
}

/// 极简百分号编码（查询串安全字符集）。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
