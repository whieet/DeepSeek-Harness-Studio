# DeepSeek Harness 桌面版（dsh-desktop）

用 [Tauri](https://tauri.app) 实现的 DeepSeek Harness 桌面客户端：原生窗口 + 系统 WebView（macOS WKWebView / Windows WebView2），加载本机运行的 `dsh web` 内核（仅绑定 127.0.0.1 随机端口）。**随包内置 Node.js 运行时与 `@deepseek-ai/dsh` 依赖闭包，用户机器无需安装 Node/npm。**

## 架构

```
┌─────────────────────── DeepSeek Harness.app / .exe ────────────────────────┐
│ Tauri 壳（Rust）                                                           │
│  ├─ 单实例守卫                                                              │
│  ├─ 状态机: Starting → Ready(url) ⇄ Restarting(1s/2s/4s 退避) ⇄ Failed     │
│  ├─ 内核进程: dsh-node …/bin.js --profile web --port 0 --no-open           │
│  ├─ 主窗口 = splash（tauri://）就绪后导航到 http://127.0.0.1:<port>         │
│  ├─ 导航围栏: 仅放行内核同源；外部链接交系统浏览器                          │
│  └─ 菜单: 工作目录 / 浏览器打开 / 重启内核 / 复制诊断 / 日志 / 退出         │
│ 资源 resources/dsh（453 包运行时闭包）  侧车 dsh-node（Node v24.20.0 LTS）  │
└────────────── WebView ←→ http://127.0.0.1:<port>（DSH Web UI）──────────────┘
```

- 内核与 CLI 完全同构（`dsh web`），`DSH_HOME` 默认 `~/.dsh`（Windows `%USERPROFILE%\.dsh`），与命令行共享凭据、模型配置与会话历史。
- 关闭窗口 = 优雅停止内核（TERM → 5s → KILL；Windows taskkill 树），无残留进程。
- 主页面零 Tauri IPC（未注入任何桥接 API），仅 splash 窗口持有最小权限（`core:default`）。

## 开发

依赖：Rust stable（macOS 需 Xcode，Windows 需 MSVC Build Tools）、Node ≥ 22、pnpm。

```bash
pnpm install          # 开发依赖（Tauri CLI）
pnpm stage            # 打包运行时：下载 Node v24.20.0 + npm 闭包 + 内核冒烟（首次必跑）
pnpm dev              # 开发运行（默认即使用打包好的 sidecar + 资源）
pnpm test             # cargo test（状态机单测 + 假内核集成测试）
```

开发模式覆盖（可选）：让壳改用系统 node 与本机 dsh 安装，便于调试内核：

```bash
DSH_DESKTOP_DEV=1 DSH_APP_DIR=/opt/homebrew/lib/node_modules/@deepseek-ai/dsh pnpm dev
```

## 打包

```bash
pnpm build            # 当前平台（macOS arm64 产出 .app/.dmg；Windows 产出 NSIS .exe）
pnpm build_x64        # macOS Intel（需 x86_64 工具链，通常走 CI）
pnpm build_win        # Windows x64
```

三平台产物由 `.github/workflows/release.yml` 矩阵构建（macos-14 arm64 / macos-13 x64 / windows-latest），打 `v*` tag 触发并上传 Release。

## 首次使用

1. 安装并启动 **DeepSeek Harness**。
2. 首启会在 `~/.dsh/profiles/web` 自动初始化 web profile（与 CLI 一致）。
3. 在 DSH 设置页配置模型 API Key（或沿用已有的 `~/.dsh/.credentials.yaml`）。
4. 应用菜单「更改工作目录…」选择默认工作区（默认为主目录；会话内仍可自由切换工作区）。

## 常见问题

- **端口冲突？** 不会。桌面版使用 `--port 0`（操作系统分配空闲端口），与 CLI 的 3080 并存互不干扰。
- **内核崩溃？** 自动按 1s/2s/4s 退避重启 3 次；仍失败则显示错误与「重试」，日志见「打开日志目录」（`dsh.log` 为内核输出）。
- **Windows 提示缺 WebView2？** 安装 [WebView2 运行时](https://developer.microsoft.com/microsoft-edge/webview2/)（Evergreen，Win10/11 通常已内置）。
- **macOS 提示无法打开？** 当前构建未做签名/公证，右键 →「打开」或在「系统设置 → 隐私与安全性」中放行。
- **多开？** 单实例守卫：二次启动只会聚焦已有窗口。

## 路线图

- [ ] Apple 签名/公证 + Windows 代码签名（阶段二）
- [ ] tauri-plugin-updater 自动更新（依赖签名）
- [ ] 托盘、`dsh://` 深链、独立日志窗口
- [ ] universal .app（需解决 per-arch 原生二进制合并）

## 许可

MIT（见 LICENSE / NOTICE）。内置的 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 与 Node.js 均为 MIT。
