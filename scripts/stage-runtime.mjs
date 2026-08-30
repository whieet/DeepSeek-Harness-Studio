// 运行时打包脚本：按当前宿主平台下载 Node 运行时 + 安装 dsh 依赖闭包 + 内核冒烟自检。
// 用法：node scripts/stage-runtime.mjs [--skip-smoke]
import { execFileSync, spawn } from "node:child_process";
import {
  chmodSync, copyFileSync, existsSync, mkdtempSync, mkdirSync, readdirSync,
  rmSync, statSync, writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const NODE_VERSION = "v24.20.0";       // 随包 Node LTS
const DSH_VERSION = "0.1.1-rc.2";      // 锁定内核版本（npm latest）
const PKG = "@deepseek-ai/dsh";
const URL_MARK = "dsh web: http://127.0.0.1:";

const SKIP_SMOKE = process.argv.includes("--skip-smoke");

// ── 平台 → 目标三元组 / Node 产物 ────────────────────────────────────────────
const arch = process.arch;
let triple;
let archiveUrl;
let archiveDir;
let nodeInside;
if (process.platform === "darwin" && arch === "arm64") {
  triple = "aarch64-apple-darwin";
  archiveDir = "node-" + NODE_VERSION + "-darwin-arm64";
  archiveUrl = "https://nodejs.org/dist/" + NODE_VERSION + "/node-" + NODE_VERSION + "-darwin-arm64.tar.gz";
  nodeInside = "bin/node";
} else if (process.platform === "darwin" && arch === "x64") {
  triple = "x86_64-apple-darwin";
  archiveDir = "node-" + NODE_VERSION + "-darwin-x64";
  archiveUrl = "https://nodejs.org/dist/" + NODE_VERSION + "/node-" + NODE_VERSION + "-darwin-x64.tar.gz";
  nodeInside = "bin/node";
} else if (process.platform === "win32" && arch === "x64") {
  triple = "x86_64-pc-windows-msvc";
  archiveDir = "node-" + NODE_VERSION + "-win-x64";
  archiveUrl = "https://nodejs.org/dist/" + NODE_VERSION + "/node-" + NODE_VERSION + "-win-x64.zip";
  nodeInside = "node.exe";
} else {
  console.error("stage-runtime: 不支持的平台/架构：" + process.platform + "/" + arch);
  process.exit(1);
}

const binDir = join(ROOT, "src-tauri", "binaries");
const sidecarName = "dsh-node-" + triple + (process.platform === "win32" ? ".exe" : "");
const sidecarPath = join(binDir, sidecarName);
const runtimeDir = join(ROOT, "src-tauri", "resources", "dsh");

function log(msg) { console.log("[stage] " + msg); }

// ── 1) Node 运行时 ────────────────────────────────────────────────────────────
async function downloadNode() {
  if (existsSync(sidecarPath)) {
    log("Node 侧车已存在：" + sidecarName + "（跳过下载）");
    return;
  }
  mkdirSync(binDir, { recursive: true });
  const work = mkdtempSync(join(tmpdir(), "dsh-node-"));
  const archive = join(work, "node." + (archiveUrl.endsWith(".zip") ? "zip" : "tar.gz"));
  log("下载 Node " + NODE_VERSION + "：" + archiveUrl);
  const res = await fetch(archiveUrl, { redirect: "follow" });
  if (!res.ok) {
    console.error("下载失败：" + archiveUrl + " → HTTP " + res.status);
    process.exit(1);
  }
  const data = Buffer.from(await res.arrayBuffer());
  writeFileSync(archive, data);
  log("解压 " + archive);
  execFileSync("tar", ["-xf", archive, "-C", work], { stdio: "inherit" });
  const src = join(work, archiveDir, nodeInside);
  if (!existsSync(src)) {
    console.error("解压后未找到：" + src);
    process.exit(1);
  }
  copyFileSync(src, sidecarPath);
  if (process.platform !== "win32") chmodSync(sidecarPath, 0o755);
  if (process.platform === "darwin") {
    // arm64 上未签名的二进制不可执行，ad-hoc 签名即可。
    try {
      execFileSync("codesign", ["--force", "--sign", "-", sidecarPath], { stdio: "inherit" });
    } catch (e) {
      console.error("codesign 失败（可忽略，仅本机调试）：" + e.message);
    }
  }
  rmSync(work, { recursive: true, force: true });
  log("Node 侧车就绪：" + sidecarPath + "（" + Math.round(statSync(sidecarPath).size / 1024 / 1024) + "MB）");
}

// ── 2) dsh 运行时闭包 ─────────────────────────────────────────────────────────
// npm 调用统一走这里：
// - Windows 上 npm 是 npm.cmd，Node 因 CVE-2024-27980 禁止无 shell 直接 spawn，
//   故优先用 node 直接执行 npm-cli.js；
// - npm-cli.js 位置优先按 node 安装布局推算（setup-node/win 官方包），推算不到时
//   用一次轻量 shell 探测（npm root -g）定位（如 Homebrew 的 Cellar 布局）；
// - CI 大闭包（450+ 包）解析吃内存，给 npm 子进程加大 V8 堆并限制并发套接字。
function npmCliJsPath() {
  const relativeCli = join("node_modules", "npm", "bin", "npm-cli.js");
  const candidates = process.platform === "win32"
    ? [join(dirname(process.execPath), relativeCli)]
    : [
        join(dirname(process.execPath), "..", "lib", relativeCli),
        join(dirname(process.execPath), relativeCli)
      ];
  for (const candidate of candidates) {
    if (existsSync(candidate)) return candidate;
  }
  try {
    const globalRoot = execFileSync("npm", ["root", "-g"], {
      encoding: "utf8",
      shell: process.platform === "win32"
    }).trim();
    const viaRoot = join(globalRoot, "npm", "bin", "npm-cli.js");
    if (existsSync(viaRoot)) return viaRoot;
  } catch {
    // 探测失败则交给 shell 兜底
  }
  return null;
}

function runNpm(args) {
  const cli = npmCliJsPath();
  const nodeOptions = ((process.env.NODE_OPTIONS || "") + " --max-old-space-size=5120").trim();
  const env = { ...process.env, NODE_OPTIONS: nodeOptions };
  if (cli) {
    execFileSync(process.execPath, [cli, ...args], { stdio: "inherit", env });
  } else {
    log("未定位到 npm-cli.js，回退 shell 方式调用 npm");
    execFileSync("npm", args, { stdio: "inherit", env, shell: process.platform === "win32" });
  }
}

function installRuntime() {
  rmSync(join(runtimeDir, "node_modules"), { recursive: true, force: true });
  rmSync(join(runtimeDir, "package.json"), { force: true });
  mkdirSync(runtimeDir, { recursive: true });
  log("npm install " + PKG + "@" + DSH_VERSION + "（运行时闭包，omit=dev）…");
  runNpm([
    "install", "--omit=dev", "--no-audit", "--no-fund", "--loglevel=warn",
    "--maxsockets", "10",
    "--prefix", runtimeDir, PKG + "@" + DSH_VERSION
  ]);
  const binJs = join(runtimeDir, "node_modules", "@deepseek-ai", "dsh", "lib", "bin.js");
  if (!existsSync(binJs)) {
    console.error("运行时闭包缺少 " + binJs);
    process.exit(1);
  }
  if (process.platform === "win32") {
    // Windows 上 koffi 依赖 install 脚本落位原生预编译产物；显式 rebuild 确保不依赖 npm allowScripts 行为。
    log("npm rebuild koffi（确保原生预编译产物就位）…");
    runNpm(["rebuild", "koffi", "--foreground-scripts", "--loglevel=warn", "--prefix", runtimeDir]);
  }
  const beforeSize = dirSize(runtimeDir);
  pruneArtifacts(runtimeDir);
  const afterSize = dirSize(runtimeDir);
  log("非运行时文件裁剪：" + fmtSize(beforeSize) + " → " + fmtSize(afterSize));
}

function dirSize(dir) {
  let total = 0;
  const walk = (d) => {
    for (const name of readdirSync(d)) {
      const p = join(d, name);
      const s = statSync(p);
      if (s.isDirectory()) walk(p); else total += s.size;
    }
  };
  walk(dir);
  return total;
}

function pruneArtifacts(dir) {
  const walk = (d) => {
    for (const name of readdirSync(d)) {
      const p = join(d, name);
      const s = statSync(p);
      if (s.isDirectory()) {
        if (name === ".bin" || name === ".cache") {
          rmSync(p, { recursive: true, force: true });
        } else {
          walk(p);
        }
      } else if (name.endsWith(".map")) {
        rmSync(p, { force: true });
      }
    }
  };
  walk(dir);
}

function fmtSize(bytes) {
  return (bytes / 1024 / 1024).toFixed(1) + "MB";
}

// ── 3) 内核冒烟自检 ───────────────────────────────────────────────────────────
function parseReadyUrl(out) {
  const idx = out.indexOf(URL_MARK);
  if (idx < 0) return null;
  let digits = "";
  for (const ch of out.slice(idx + URL_MARK.length)) {
    if (ch >= "0" && ch <= "9") digits += ch; else break;
  }
  return digits === "" ? null : "http://127.0.0.1:" + digits + "/";
}

async function smoke() {
  const binJs = join(runtimeDir, "node_modules", "@deepseek-ai", "dsh", "lib", "bin.js");
  const home = mkdtempSync(join(tmpdir(), "dsh-smoke-"));
  log("冒烟自检：临时 DSH_HOME=" + home);
  const child = spawn(sidecarPath, [binJs, "--profile", "web", "--port", "0", "--no-open"], {
    cwd: home,
    env: { ...process.env, DSH_HOME: home },
    stdio: ["ignore", "pipe", "pipe"]
  });
  let out = "";
  let err = "";
  child.stdout.on("data", (c) => { out += c; });
  child.stderr.on("data", (c) => { err += c; });
  const deadline = Date.now() + 120000;
  let url = null;
  while (Date.now() < deadline && child.exitCode === null) {
    url = parseReadyUrl(out);
    if (url !== null) break;
    await new Promise((r) => setTimeout(r, 250));
  }
  if (url === null) {
    console.error("冒烟失败：未在 120s 内等到就绪 URL。");
    console.error("stdout 尾部：");
    console.error(out.slice(-2000));
    console.error("stderr 尾部：");
    console.error(err.slice(-2000));
    child.kill();
    process.exit(1);
  }
  log("内核就绪：" + url + "，等待 HTTP 200…");
  const resp = await fetch(url);
  if (resp.status !== 200) {
    console.error("冒烟失败：GET " + url + " → " + resp.status);
    child.kill();
    process.exit(1);
  }
  log("HTTP 200 OK —— 内核冒烟通过");
  child.kill();
  await new Promise((r) => setTimeout(r, 500));
  rmSync(home, { recursive: true, force: true });
}

// ── 主流程 ────────────────────────────────────────────────────────────────────
log("平台：" + process.platform + "/" + arch + " → " + triple);
await downloadNode();
installRuntime();
log("运行时闭包大小：" + fmtSize(dirSize(runtimeDir)));
if (!SKIP_SMOKE) {
  await smoke();
} else {
  log("--skip-smoke：跳过冒烟自检");
}
log("完成。产物：");
log("  " + relative(ROOT, sidecarPath));
log("  " + relative(ROOT, runtimeDir));
