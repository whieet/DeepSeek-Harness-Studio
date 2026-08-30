// dsh-desktop 内核运行时更新器：由壳（Rust）以随包 Node 侧车执行。
// 子命令：
//   check --kernel-current <v> --app-current <v> [--node-version <v>]
//     检查两条更新通道：内核（npm registry 的 @deepseek-ai/dsh）与客户端
//     （GitHub Releases，只读）。stdout 输出一行 JSON 结果。
//   apply --version <v> --dest <runtimeDir> --npm <npm-cli.js> [--cache <dir>] [--dry-run]
//     在用户机器上装配新内核运行时：按需下载 Node（SHASUMS256 校验）→
//     npm 安装闭包（--ignore-scripts，原生模块全部为 prebuilt 平台包）→
//     冒烟自检 → 原子落位覆盖层并写 meta.json。
//     stdout 输出 JSON 行进度；人类可读日志走 stderr。退出码 0 成功 / 1 失败。
//
// 协议约定（供 Rust 壳解析）：
//   check  → stdout 单行 { kernel: {...}, app: {...} }
//   apply  → stdout 多行 { event: "progress" | "error" | "done", ... }

import { createHash } from "node:crypto";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import {
  chmodSync, copyFileSync, cpSync, existsSync, mkdirSync, mkdtempSync,
  readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

const REGISTRY_LATEST = "https://registry.npmjs.org/@deepseek-ai/dsh/latest";
const REGISTRY_PKG = "https://registry.npmjs.org/@deepseek-ai/dsh";
const APP_RELEASES_API =
  "https://api.github.com/repos/whieet/DeepSeek-Harness-Studio/releases/latest";
const NODE_DIST = "https://nodejs.org/dist";
const PKG = "@deepseek-ai/dsh";
const URL_MARK = "dsh web: http://127.0.0.1:";
const SMOKE_TIMEOUT_MS = 120000;

// ── 极简 semver（覆盖 engines 常见写法，零依赖） ─────────────────────────────

export function parseVersion(input) {
  let v = String(input ?? "").trim();
  if (v.startsWith("v") || v.startsWith("V")) v = v.slice(1);
  const plus = v.indexOf("+");
  if (plus >= 0) v = v.slice(0, plus);
  const dash = v.indexOf("-");
  let core = v;
  let preText = "";
  if (dash >= 0) {
    core = v.slice(0, dash);
    preText = v.slice(dash + 1);
  }
  const parts = core.split(".");
  if (parts.length !== 3) return null;
  const nums = [];
  for (const part of parts) {
    if (!/^[0-9]+$/.test(part)) return null;
    nums.push(Number(part));
  }
  return {
    major: nums[0],
    minor: nums[1],
    patch: nums[2],
    prerelease: preText === "" ? [] : preText.split("."),
  };
}

function cmp(a, b) { return a < b ? -1 : a > b ? 1 : 0; }

function cmpPrereleaseIds(x, y) {
  const xn = /^[0-9]+$/.test(x);
  const yn = /^[0-9]+$/.test(y);
  if (xn && yn) return cmp(Number(x), Number(y));
  if (xn) return -1; // 数字段 < 字母段（semver 规则）
  if (yn) return 1;
  return cmp(x, y);
}

function cmpVersionObjects(a, b) {
  for (const key of ["major", "minor", "patch"]) {
    if (a[key] !== b[key]) return cmp(a[key], b[key]);
  }
  const pa = a.prerelease ?? [];
  const pb = b.prerelease ?? [];
  if (pa.length === 0 && pb.length === 0) return 0;
  if (pa.length === 0) return 1;  // 正式发布 > 预发布
  if (pb.length === 0) return -1;
  const len = Math.max(pa.length, pb.length);
  for (let i = 0; i < len; i += 1) {
    if (i >= pa.length) return -1;
    if (i >= pb.length) return 1;
    const ord = cmpPrereleaseIds(pa[i], pb[i]);
    if (ord !== 0) return ord;
  }
  return 0;
}

/** 返回 -1/0/1；任一版本无法解析返回 null（调用方视为“无更新”）。 */
export function compareVersions(a, b) {
  const va = parseVersion(a);
  const vb = parseVersion(b);
  if (!va || !vb) return null;
  return cmpVersionObjects(va, vb);
}

/** 解析单个比较符：^1.2.3 / ~1.2 / >=20 / 1.x / * 等。 */
function parseComparator(text) {
  let s = String(text ?? "").trim();
  let op = "";
  for (const candidate of [">=", "<=", "^", "~", ">", "<", "="]) {
    if (s.startsWith(candidate)) {
      op = candidate;
      s = s.slice(candidate.length);
      break;
    }
  }
  if (op === "=") op = "";
  if (s.startsWith("v") || s.startsWith("V")) s = s.slice(1);
  const dash = s.indexOf("-");
  let core = s;
  let preText = "";
  if (dash >= 0) {
    core = s.slice(0, dash);
    preText = s.slice(dash + 1);
  }
  const rawParts = core.split(".");
  const isX = (t) => t === undefined || t === "x" || t === "X" || t === "*" || t === "";
  const numOf = (t) => {
    if (isX(t)) return null;
    return /^[0-9]+$/.test(t) ? Number(t) : NaN;
  };
  const major = numOf(rawParts[0]);
  if (Number.isNaN(major)) return null;
  const minor = numOf(rawParts[1]);
  if (Number.isNaN(minor)) return null;
  const patch = numOf(rawParts[2]);
  if (Number.isNaN(patch)) return null;
  return { op, major, minor, patch, prerelease: preText === "" ? [] : preText.split(".") };
}

function satisfiesBound(version, bound) {
  const ord = cmpVersionObjects(version, bound.version);
  switch (bound.op) {
    case ">": return ord > 0;
    case ">=": return ord >= 0;
    case "<": return ord < 0;
    case "<=": return ord <= 0;
    default: return ord === 0;
  }
}

function caretTildeBounds(spec, op) {
  if (op === "^") {
    let up;
    if (spec.major > 0) up = { major: spec.major + 1, minor: 0, patch: 0, prerelease: ["0"] };
    else if (spec.minor > 0) up = { major: spec.major, minor: spec.minor + 1, patch: 0, prerelease: ["0"] };
    else up = { major: spec.major, minor: spec.minor, patch: spec.patch + 1, prerelease: ["0"] };
    return [{ op: ">=", version: spec }, { op: "<", version: up }];
  }
  const up = { major: spec.major, minor: spec.minor + 1, patch: 0, prerelease: ["0"] };
  return [{ op: ">=", version: spec }, { op: "<", version: up }];
}

function satisfiesComparator(version, compText) {
  const c = parseComparator(compText);
  if (!c) return false;
  if (c.major === null) return true; // "*"

  // x-range：1.x / 1.2.x / 1（无操作符且带通配段）
  if (c.op === "" && (c.minor === null || c.patch === null)) {
    if (version.prerelease.length > 0) return false;
    const lower = { major: c.major, minor: c.minor ?? 0, patch: 0, prerelease: [] };
    const upper = c.minor === null
      ? { major: c.major + 1, minor: 0, patch: 0, prerelease: ["0"] }
      : { major: c.major, minor: c.minor + 1, patch: 0, prerelease: ["0"] };
    return cmpVersionObjects(version, lower) >= 0 && cmpVersionObjects(version, upper) < 0;
  }

  const spec = {
    major: c.major,
    minor: c.minor ?? 0,
    patch: c.patch ?? 0,
    prerelease: c.prerelease,
  };

  // semver 规则：带 prerelease 的版本，仅在比较符声明了同元组 prerelease 时才可满足
  const tuple = { major: c.major, minor: c.minor ?? 0, patch: c.patch ?? 0, prerelease: [] };
  const sameTupleWithPre = c.prerelease.length > 0
    && cmpVersionObjects(tuple, { major: version.major, minor: version.minor, patch: version.patch, prerelease: [] }) === 0;
  if (version.prerelease.length > 0 && c.prerelease.length === 0 && !sameTupleWithPre) return false;

  if (c.op === ">" || c.op === ">=" || c.op === "<" || c.op === "<=") {
    return satisfiesBound(version, { op: c.op, version: spec });
  }
  if (c.op === "^" || c.op === "~") {
    return caretTildeBounds(spec, c.op).every((b) => satisfiesBound(version, b));
  }
  return cmpVersionObjects(version, spec) === 0; // "=" 或精确写法
}

export function satisfiesRange(versionText, range) {
  const version = parseVersion(versionText);
  if (!version) return false;
  const text = String(range ?? "").trim();
  if (text === "" || text === "*" || text === "latest") return true;
  return text.split("||").some((group) => {
    const comps = group.trim().split(" ").filter((t) => t !== "");
    if (comps.length === 0) return true;
    return comps.every((c) => satisfiesComparator(version, c));
  });
}

// ── 工具 ─────────────────────────────────────────────────────────────────────

function log(msg) { process.stderr.write("[update] " + msg + "\n"); }
function emit(event) { console.log(JSON.stringify(event)); }
function progress(phase, pct, message) { emit({ event: "progress", phase, pct, message }); }
function die(message) { emit({ event: "error", message }); process.exit(1); }

async function fetchJson(url, headers = {}) {
  const res = await fetch(url, {
    redirect: "follow",
    headers: { "user-agent": "dsh-desktop-updater", ...headers },
    signal: AbortSignal.timeout(20000),
  });
  let json = null;
  try { json = await res.json(); } catch { /* 非 JSON 响应保持 null */ }
  return { status: res.status, json };
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (!arg.startsWith("--")) continue;
    const key = arg.slice(2);
    const next = argv[i + 1];
    if (next === undefined || next.startsWith("--")) out[key] = true;
    else { out[key] = next; i += 1; }
  }
  return out;
}

// ── check：两通道版本检查 ────────────────────────────────────────────────────

async function cmdCheck(args) {
  const kernelCurrent = args["kernel-current"] ?? "";
  const appCurrent = args["app-current"] ?? "";
  const nodeVersion = args["node-version"] ?? process.version;

  // 内核通道：npm registry latest（含 engines 声明）
  let kernel;
  try {
    const { status, json } = await fetchJson(REGISTRY_LATEST);
    if (status !== 200 || !json || !json.version) {
      kernel = { current: kernelCurrent, latest: null, updateAvailable: false, reason: "http-" + status };
    } else {
      const latest = json.version;
      const enginesNode = (json.engines && json.engines.node) ? json.engines.node : null;
      const ord = compareVersions(latest, kernelCurrent);
      kernel = {
        current: kernelCurrent,
        latest,
        updateAvailable: ord !== null && ord > 0,
        enginesNode,
        nodeSufficient: enginesNode ? satisfiesRange(nodeVersion, enginesNode) : null,
      };
    }
  } catch (e) {
    kernel = { current: kernelCurrent, latest: null, updateAvailable: false, reason: "unreachable: " + (e && e.message ? e.message : e) };
  }

  // 客户端通道：GitHub Releases（只读；无 Release / 限流时优雅降级）
  let app;
  try {
    const { status, json } = await fetchJson(APP_RELEASES_API, { accept: "application/vnd.github+json" });
    if (status === 404) {
      app = { current: appCurrent, latest: null, updateAvailable: false, reason: "no-release", url: null };
    } else if (status !== 200 || !json || !json.tag_name) {
      app = { current: appCurrent, latest: null, updateAvailable: false, reason: "http-" + status, url: null };
    } else {
      const latest = String(json.tag_name).replace(/^v/i, "");
      const ord = compareVersions(latest, appCurrent);
      app = {
        current: appCurrent,
        latest,
        updateAvailable: ord !== null && ord > 0,
        url: json.html_url ?? null,
      };
    }
  } catch (e) {
    app = { current: appCurrent, latest: null, updateAvailable: false, reason: "unreachable: " + (e && e.message ? e.message : e), url: null };
  }

  console.log(JSON.stringify({ kernel, app }));
}

// ── Node 产物（apply 按需下载，SHASUMS256 校验） ─────────────────────────────

function distTargets(nodeVersion) {
  const v = nodeVersion.replace(/^v/i, "");
  const arch = process.arch;
  if (process.platform === "darwin" && arch === "arm64") {
    return { file: "node-v" + v + "-darwin-arm64.tar.gz", dir: "node-v" + v + "-darwin-arm64", nodeInside: "bin/node" };
  }
  if (process.platform === "darwin" && arch === "x64") {
    return { file: "node-v" + v + "-darwin-x64.tar.gz", dir: "node-v" + v + "-darwin-x64", nodeInside: "bin/node" };
  }
  if (process.platform === "win32" && arch === "x64") {
    return { file: "node-v" + v + "-win-x64.zip", dir: "node-v" + v + "-win-x64", nodeInside: "node.exe" };
  }
  return null;
}

async function pickNodeDistVersion(range) {
  const { status, json } = await fetchJson(NODE_DIST + "/index.json");
  if (status !== 200 || !Array.isArray(json)) return null;
  const ok = (entry) => entry.version && satisfiesRange(entry.version, range);
  return json.find((e) => e.lts && ok(e)) ?? json.find((e) => ok(e)) ?? null;
}

async function downloadNode(nodeVersion, targetPath) {
  const targets = distTargets(nodeVersion);
  if (!targets) die("不支持的平台/架构：" + process.platform + "/" + process.arch);
  const work = mkdtempSync(join(tmpdir(), "dsh-update-node-"));
  try {
    const archivePath = join(work, targets.file);
    log("下载 Node " + nodeVersion + "：" + NODE_DIST + "/" + nodeVersion + "/" + targets.file);
    const res = await fetch(NODE_DIST + "/" + nodeVersion + "/" + targets.file, {
      redirect: "follow", signal: AbortSignal.timeout(600000),
    });
    if (!res.ok) die("Node 下载失败：HTTP " + res.status);
    const data = Buffer.from(await res.arrayBuffer());

    const shaRes = await fetch(NODE_DIST + "/" + nodeVersion + "/SHASUMS256.txt", {
      redirect: "follow", signal: AbortSignal.timeout(30000),
    });
    if (!shaRes.ok) die("SHASUMS256.txt 下载失败：HTTP " + shaRes.status);
    const lines = (await shaRes.text()).split("\n").map((l) => l.trim());
    const shaLine = lines.find((l) => l.split(" ").filter((t) => t !== "").length === 2 && l.split(/\s+/)[1] === targets.file);
    if (!shaLine) die("SHASUMS256.txt 中缺少 " + targets.file);
    const expected = shaLine.split(/\s+/)[0].toLowerCase();
    const actual = createHash("sha256").update(data).digest("hex");
    if (expected !== actual) die("Node 产物 SHASUMS256 校验失败");

    writeFileSync(archivePath, data);
    log("解压 " + archivePath);
    execFileSync("tar", ["-xf", archivePath, "-C", work], { stdio: "inherit" });
    const src = join(work, targets.dir, targets.nodeInside);
    if (!existsSync(src)) die("解压后未找到 " + src);
    mkdirSync(dirname(targetPath), { recursive: true });
    copyFileSync(src, targetPath);
    if (process.platform !== "win32") chmodSync(targetPath, 0o755);
    if (process.platform === "darwin") {
      try { execFileSync("codesign", ["--force", "--sign", "-", targetPath], { stdio: "inherit" }); }
      catch { /* 无签名环境忽略 */ }
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

// ── npm 安装闭包 ─────────────────────────────────────────────────────────────

function runNpm(nodeBin, npmCli, args, cwd, cacheDir) {
  const env = {
    ...process.env,
    npm_config_cache: cacheDir,
    NODE_OPTIONS: ((process.env.NODE_OPTIONS || "") + " --max-old-space-size=5120").trim(),
  };
  const res = spawnSync(nodeBin, [npmCli, ...args], { cwd, env, encoding: "utf8" });
  return {
    status: res.status,
    stderr: res.stderr ?? "",
    stdout: res.stdout ?? "",
  };
}

function pruneArtifacts(dir) {
  const walk = (d) => {
    let entries;
    try { entries = readdirSync(d); } catch { return; }
    for (const name of entries) {
      const p = join(d, name);
      let stat;
      try { stat = statSync(p); } catch { continue; }
      if (stat.isDirectory()) {
        if (name === ".bin" || name === ".cache") rmSync(p, { recursive: true, force: true });
        else walk(p);
      } else if (name.endsWith(".map")) {
        rmSync(p, { force: true });
      }
    }
  };
  walk(dir);
}

// ── 冒烟自检（临时 DSH_HOME，不触碰用户数据） ────────────────────────────────

function parseReadyPort(text) {
  const idx = text.indexOf(URL_MARK);
  if (idx < 0) return null;
  let digits = "";
  for (const ch of text.slice(idx + URL_MARK.length)) {
    if (ch >= "0" && ch <= "9") digits += ch;
    else break;
  }
  return digits === "" ? null : digits;
}

async function smoke(nodeBin, binJs) {
  const home = mkdtempSync(join(tmpdir(), "dsh-update-smoke-"));
  const child = spawn(nodeBin, [binJs, "--profile", "web", "--port", "0", "--no-open"], {
    cwd: home,
    env: { ...process.env, DSH_HOME: home },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let out = "";
  let err = "";
  child.stdout.on("data", (c) => { out += c; });
  child.stderr.on("data", (c) => { err += c; });
  try {
    const deadline = Date.now() + SMOKE_TIMEOUT_MS;
    let port = null;
    while (Date.now() < deadline && child.exitCode === null) {
      port = parseReadyPort(out);
      if (port !== null) break;
      await new Promise((r) => setTimeout(r, 250));
    }
    if (port === null) {
      return "冒烟失败：未在 " + Math.round(SMOKE_TIMEOUT_MS / 1000) + "s 内等到就绪 URL。stderr 尾部：" + err.slice(-1500);
    }
    const resp = await fetch("http://127.0.0.1:" + port + "/", { signal: AbortSignal.timeout(10000) });
    if (resp.status !== 200) {
      return "冒烟失败：GET / 返回 " + resp.status;
    }
    return null;
  } finally {
    try { child.kill(); } catch { /* 已退出 */ }
    await new Promise((r) => setTimeout(r, 500));
    rmSync(home, { recursive: true, force: true });
  }
}

// ── apply：装配新运行时 ──────────────────────────────────────────────────────

async function cmdApply(args) {
  const version = String(args.version ?? "");
  const dest = args.dest ?? "";
  const npmCli = args.npm ?? "";
  const cacheDir = args.cache ?? join(dirname(dest), "update-cache");
  const dryRun = args["dry-run"] === true;
  if (!parseVersion(version)) die("非法版本号：" + version);
  if (dest === "" || npmCli === "") die("缺少 --dest 或 --npm 参数");
  const cleanVersion = version.replace(/^v/i, "");
  mkdirSync(dest, { recursive: true });

  // 幂等：同版本已就位直接成功。
  try {
    const meta = JSON.parse(readFileSync(join(dest, "meta.json"), "utf8"));
    if (meta.status === "ready" && meta.dshVersion === cleanVersion) {
      emit({ event: "done", dshVersion: cleanVersion, nodeVersion: meta.nodeVersion ?? null, already: true });
      return;
    }
  } catch { /* 无 meta 或不可读，继续 */ }

  // 1) 目标版本 engines → 是否需要更新 Node
  progress("node", 5, "查询目标版本元数据…");
  const { status, json: pkg } = await fetchJson(REGISTRY_PKG + "/" + cleanVersion);
  if (status !== 200 || !pkg || !pkg.version) die("查询 " + PKG + "@" + cleanVersion + " 失败：HTTP " + status);
  const enginesNode = (pkg.engines && pkg.engines.node) ? pkg.engines.node : null;

  let nodeBin = process.execPath;
  let nodeVersionUsed = null;
  let nodeDownloaded = false;
  const nodeDir = join(dirname(dest), "node-new");
  if (enginesNode && !satisfiesRange(process.version, enginesNode)) {
    const picked = await pickNodeDistVersion(enginesNode);
    if (!picked || !picked.version) die("没有满足 engines '" + enginesNode + "' 的 Node 发行版");
    nodeVersionUsed = picked.version;
    if (dryRun) {
      console.log(JSON.stringify({ dryRun: true, version: cleanVersion, enginesNode, currentNode: process.version, newNode: picked.version }));
      return;
    }
    progress("node", 15, "当前 Node " + process.version + " 不满足 " + enginesNode + "，下载 " + picked.version + "…");
    const nodeExeName = process.platform === "win32" ? "dsh-node.exe" : "dsh-node";
    await downloadNode(picked.version, join(nodeDir, nodeExeName));
    nodeBin = join(nodeDir, nodeExeName);
    nodeDownloaded = true;
    progress("node", 25, "Node " + picked.version + " 就绪（SHASUMS256 已校验）");
  } else {
    if (dryRun) {
      console.log(JSON.stringify({ dryRun: true, version: cleanVersion, enginesNode, currentNode: process.version, newNode: null }));
      return;
    }
    progress("node", 20, "内置 Node " + process.version + " 满足要求" + (enginesNode ? "（engines " + enginesNode + "）" : ""));
  }

  // 2) npm 安装闭包：默认 --ignore-scripts（原生模块全为 prebuilt 平台包）；
  //    冒烟失败时降级为允许脚本重装一次（与打包期同信任级别）。
  const tmpRoot = mkdtempSync(join(dirname(dest), "update-tmp-"));
  const dshTmp = join(tmpRoot, "dsh");
  mkdirSync(dshTmp, { recursive: true });
  writeFileSync(join(dshTmp, "package.json"), JSON.stringify({
    name: "dsh-runtime",
    private: true,
    dependencies: { [PKG]: cleanVersion },
  }, null, 2));

  const binJs = join(dshTmp, "node_modules", "@deepseek-ai", "dsh", "lib", "bin.js");
  let smokeError = null;

  for (const allowScripts of [false, true]) {
    rmSync(join(dshTmp, "node_modules"), { recursive: true, force: true });
    const installArgs = ["install", "--omit=dev", "--no-audit", "--no-fund", "--loglevel=warn", "--maxsockets", "10"];
    if (!allowScripts) installArgs.push("--ignore-scripts");
    progress("install", 35, (allowScripts ? "重试安装（允许安装脚本）：" : "安装：") + PKG + "@" + cleanVersion);
    const res = runNpm(nodeBin, npmCli, installArgs, dshTmp, cacheDir);
    if (res.status !== 0) {
      die("npm install 失败（exit=" + res.status + "）：" + res.stderr.slice(-1500));
    }
    if (process.platform === "win32") {
      progress("install", 50, "npm rebuild koffi（确保原生预编译产物就位）…");
      const rb = runNpm(nodeBin, npmCli, ["rebuild", "koffi", "--foreground-scripts", "--loglevel=warn"], dshTmp, cacheDir);
      if (rb.status !== 0) die("npm rebuild koffi 失败：" + rb.stderr.slice(-1000));
    }
    if (!existsSync(binJs)) die("闭包缺少内核入口 " + binJs);
    progress("install", 58, "裁剪非运行时文件…");
    pruneArtifacts(dshTmp);

    progress("smoke", 70, "冒烟自检：启动新内核…");
    smokeError = await smoke(nodeBin, binJs);
    if (smokeError === null) break;
    if (allowScripts) break;
    log("首次冒烟未通过，降级重试：" + smokeError);
  }
  if (smokeError !== null) die(smokeError);
  progress("smoke", 82, "冒烟通过（HTTP 200）");

  // 3) 原子落位：meta(staging) → 换目录 → meta(ready)
  progress("finalize", 90, "落位覆盖层…");
  const dshFinal = join(dest, "dsh");
  const nodeFinal = join(dest, "node");
  const writeMeta = (st) => writeFileSync(join(dest, "meta.json"), JSON.stringify({
    dshVersion: cleanVersion,
    nodeVersion: nodeVersionUsed,
    stagedAt: new Date().toISOString(),
    status: st,
  }, null, 2));
  writeMeta("staging");
  if (existsSync(dshFinal)) renameSync(dshFinal, join(tmpRoot, "dsh.old"));
  if (nodeDownloaded) {
    if (existsSync(nodeFinal)) renameSync(nodeFinal, join(tmpRoot, "node.old"));
    renameSync(nodeDir, nodeFinal);
  } else if (existsSync(nodeFinal)) {
    rmSync(nodeFinal, { recursive: true, force: true });
  }
  renameSync(dshTmp, dshFinal);
  writeMeta("ready");
  rmSync(tmpRoot, { recursive: true, force: true });
  if (existsSync(nodeDir)) rmSync(nodeDir, { recursive: true, force: true });

  emit({ event: "done", dshVersion: cleanVersion, nodeVersion: nodeVersionUsed });
}

// ── 入口 ─────────────────────────────────────────────────────────────────────

async function main() {
  const argv = process.argv.slice(2);
  const command = argv[0] ?? "";
  const args = parseArgs(argv.slice(1));
  if (command === "check") await cmdCheck(args);
  else if (command === "apply") await cmdApply(args);
  else die("未知子命令：" + command + "（可用：check / apply）");
}

// 供 node --test 导入；直接执行时才进入 CLI（测试文件名 update.test.mjs 不匹配）。
if (process.argv[1] && process.argv[1].endsWith("update.mjs")) {
  main().catch((e) => die(String(e && e.stack ? e.stack : e)));
}
