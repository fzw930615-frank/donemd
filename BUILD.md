# Done.md — 本地构建

## 前提条件

- macOS 13+（Ventura 或更新）
- Xcode 14+（从 Mac App Store 下载）
- **Node.js 18+** + npm（Web bundle 构建用，从 [nodejs.org](https://nodejs.org/) 或 `brew install node`）
- Homebrew + xcodegen（仅在改 `project.yml` 时需要）：
  ```bash
  brew install xcodegen
  ```

## 第一次拉到本机

```bash
cd ~/donemd
cd web && npm install && cd ..    # 装 web 依赖（Tiptap、Vite、TypeScript 等）
xcodegen generate                 # 从 project.yml 生成 .xcodeproj
open donemd.xcodeproj
```

Xcode 里 Cmd+R 即可 build & run。首次运行需要在 *Signing & Capabilities* 里选 Team（"Personal Team" 也 OK，Apple ID 即可，免费）。

> 国内网络如果 npm 装不动：`npm install --registry=https://registry.npmmirror.com`

## 修改工程结构

工程结构由 `project.yml` 描述，**不**直接改 `donemd.xcodeproj`——它是衍生产物，已加进 `.gitignore`。流程：

```bash
# 1. 编辑 project.yml（加文件、加 framework、改 build settings 等）
# 2. 重新生成 .xcodeproj
xcodegen generate
# 3. Xcode 里会自动重载工程
```

如果忘了走这条路、直接在 Xcode UI 里改了工程设置，下次 `xcodegen generate` 会覆盖你的改动——记得把改动反向写回 `project.yml`。

## 命令行 build & test

```bash
# 仅编译
xcodebuild -project donemd.xcodeproj -scheme donemd -configuration Debug build

# 跑单元测试
xcodebuild -project donemd.xcodeproj -scheme donemd test
```

## 工程结构

```
~/donemd/
├─ project.yml             # 工程结构真相源（git 跟踪）
├─ donemd.xcodeproj/       # 生成的 Xcode 工程（.gitignore，不进 git）
├─ donemd/                 # App 主 target
│  ├─ donemdApp.swift      # @main 入口
│  ├─ ContentView.swift    # 根视图
│  ├─ VisualWebView.swift  # WKWebView 包装，加载 visual.html
│  └─ Assets.xcassets/     # 资源目录
├─ donemdTests/            # 单元测试 target
│  └─ DonemdTests.swift
├─ web/                    # JS 源码（Vite + TS + Tiptap），macOS / Windows 共用
│  ├─ build.mjs            # 跨平台构建入口：逐页跑 vite build（VITE_ENTRY）
│  ├─ visual.html          # 多页入口：主编辑器
│  ├─ markdown-source.html # 多页入口：右侧 Markdown 源码镜像栏
│  ├─ outline.html         # 多页入口：文档大纲侧栏
│  ├─ settings.html        # 多页入口：设置窗（AI / 飞书同步）
│  ├─ src/main.ts          # Tiptap 编辑器初始化
│  ├─ src/bridge.ts        # 信封协议（WKWebView / Tauri 双传输）
│  ├─ src/visual.css       # 编辑器样式
│  └─ vite.config.ts       # 输出到 ../Resources/Web/
├─ Resources/Web/          # Vite 构建产物（.gitignore，由 pre-build 脚本生成；Windows 侧同源）
└─ BUILD.md                # 本文件
```

## Web bundle 构建流程

`project.yml` 配的 `preBuildScripts` 会让 Xcode 每次 build 前自动跑 `cd web && npm run build`
（`node build.mjs`，四个页面各跑一次 vite build）：

- 首次 build：自动跑 `npm install`（如果 `node_modules` 不存在）
- Vite 用内容 hash 做增量缓存，没改时快速退出
- 输出：`Resources/Web/{visual,markdown-source,outline,settings}.html`（CSS + JS 全 inline 的单文件；visual 约 4.6 MB，mermaid/CodeMirror 占大头）
- 这些文件以 folder reference 方式被打进 `.app/Contents/Resources/Web/`，Windows 侧则作为 `frontendDist` 内嵌进 exe

---

# Windows 构建（Tauri 2）

Windows 版是 `windows/` 下的 Tauri 2 壳，复用同一份 `web/` 编辑器产物（多页：visual / markdown-source / outline / settings），不依赖 Xcode 工具链——两端的构建互不影响。

## 前提条件

- Windows 10/11 x64；WebView2 Runtime（Win11 预装，Win10 一般也有，缺失时安装包会引导）
- [Rust stable](https://rustup.rs/)（MSVC 工具链）
- Node.js 18+ + npm
- Microsoft C++ Build Tools（Visual Studio Installer 勾选「使用 C++ 的桌面开发」）

## 工程结构（Windows）

```
windows/
├─ package.json            # 只装 @tauri-apps/cli，脚本转发到 tauri
├─ cargo-win.bat / .sh     # 带 MSVC 环境的 cargo 包装（见下节「工具链注意」）
└─ src-tauri/
   ├─ Cargo.toml           # 依赖面（tauri 开 unstable 用于多 webview）
   ├─ tauri.conf.json      # 窗口 / CSP / 打包目标（msi + nsis）/ .md 文件关联
   ├─ capabilities/        # Tauri 2 权限（默认 + 设置窗）
   ├─ icons/
   └─ src/
      ├─ main.rs           # 入口：注册命令、启动源码栏、菜单、主题
      ├─ bridge.rs         # 信封协议分发（与 web/src/bridge.ts 对偶）
      ├─ document.rs       # 新建/打开/保存/另存、脏关闭确认、argv 文件关联
      ├─ menu.rs           # 原生菜单（文件/视图/格式/插入）
      ├─ state.rs          # AppState（文档、AI、飞书各持内锁）
      ├─ layout.rs         # 三栏布局唯一真源（[大纲?][visual][source]）
      ├─ outline.rs        # 大纲侧栏 child webview
      ├─ source.rs         # 右侧 Markdown 源码镜像栏
      ├─ theme.rs          # 系统主题 → 各 webview 画布
      ├─ assets.rs         # donemd-asset:// 协议（图片读写、HTTP Range）
      ├─ markdown/         # Markdown 引擎（Swift 2100 行的 Rust 移植）
      ├─ ai/               # AI 助手（5 Provider、SSE、凭据管理器）
      └─ feishu/           # 飞书（转换层 + 网络/身份 + 凭据/设置；推拉同步移植中）
```

## 日常开发

```powershell
cd web && npm install          # 首次：装编辑器依赖
cd ../windows && npm install   # 首次：装 @tauri-apps/cli

npm run dev                    # vite dev server + tauri dev，热重载
npm run build-debug            # 独立 debug 包（不依赖 dev server，可直接双击）
                               #   → src-tauri/target/debug/donemd.exe
```

> 注意：直接 `cargo build` 出的 debug exe 指向 vite devUrl（localhost:5173），
> 不起 dev server 时窗口会「无法访问」。要独立调试包一律走 `npm run build-debug`。

## 工具链注意（Git Bash / MSVC）

在 Git Bash 里直接跑 `cargo` 可能报 `link: extra operand …`——Git Bash 自带 coreutils 的
`/usr/bin/link.exe`，会遮蔽 MSVC 链接器，rust `windows-msvc` 构建全线失败。用仓库里的包装脚本绕开
（它先把 `vcvars64.bat` 加载进 cmd，再调真正的 `cargo.exe`）：

```bash
windows/cargo-win.sh test            # bash 入口（cmd.exe /c 转发）
windows\cargo-win.bat test           # cmd / PowerShell 直接用
```

脚本用 `vswhere` 定位 VS 安装路径，找不到时回退 `%ProgramFiles%\Microsoft Visual Studio\2022\Community`。

## 测试

```powershell
cd windows/src-tauri && cargo test    # Rust 侧（markdown 引擎、资产协议、AI、飞书等）
cd web && npm test                    # vitest（桥接、doc-size、下拉布局等纯 TS 逻辑）
```

（Git Bash 下 `cargo` 报 link 错误时改用 `windows/cargo-win.sh test`，见上节。）

当前测试面（2026-09-22）：`cargo test` **360 通过**（5 个 `#[ignore]` 为需真机/真凭据的手动探针）、
`npm test` **58 通过**。Rust 侧含 macOS 测试用例的逐条移植（规范化 6 项、frontmatter 边界、
资产 Range 解析、飞书转换层等）。

## 发布构建

```powershell
cd windows
npm run build    # = tauri build：web 四页构建 → cargo release → WiX/NSIS 打包
```

产物（版本号以 `src-tauri/tauri.conf.json` 的 `version` 为准）：

- `windows/src-tauri/target/release/bundle/msi/Done.md_<version>_x64_en-US.msi`
- `windows/src-tauri/target/release/bundle/nsis/Done.md_<version>_x64-setup.exe`

版本 1.0.0 的实测体积（release profile 开了 `lto` / `codegen-units=1` / `strip` / `panic=abort`）：
release exe 7.59 MB、MSI 4.36 MB、NSIS 3.49 MB（debug exe 为 17 MB，仅用于调试）。

首次打包 Tauri 会自动下载 WiX 3.11 与 NSIS 工具链（需能访问 GitHub Releases；
网络受限时先手动下载放进 `%LOCALAPPDATA%\tauri`）。

两个安装包都会注册 `.md` / `.markdown` 文件关联（`tauri.conf.json` 的
`fileAssociations`），安装后双击 .md 直接进 Done.md。

## 版本号策略

Windows 与 macOS 共用同一营销版本号：发版时同步改两处——

- `windows/src-tauri/tauri.conf.json` → `version`
- `project.yml` → `MARKETING_VERSION`（当前两侧均为 1.0.0）

Windows 侧不需要单独 build 号；MSI 的四段版本由 Tauri 从 `version` 自动派生。

## 代码签名（当前未做）

未签名安装包首次安装/运行会触发 SmartScreen「Windows 已保护你的电脑」——
点「更多信息 → 仍要运行」即可。正式发布前再评估 EV 证书或 Azure Trusted Signing。

## CI（GitHub Actions, windows-latest）

`windows-latest` 镜像已预装 Rust、Node、WebView2、VS Build Tools，无需额外装系统依赖。
骨架（尚未启用，接 CI 时建 `.github/workflows/windows-build.yml`）：

```yaml
name: windows-build
on: [push, pull_request]
jobs:
  build:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with: { node-version: 20 }
      - uses: dtolnay/rust-toolchain@stable
      - run: npm ci --prefix web && npm ci --prefix windows
      - run: cargo test --manifest-path windows/src-tauri/Cargo.toml
      - run: npm test --prefix web
      - run: npm run build --prefix windows
      - uses: actions/upload-artifact@v4
        with:
          name: donemd-windows-installers
          path: |
            windows/src-tauri/target/release/bundle/msi/*.msi
            windows/src-tauri/target/release/bundle/nsis/*.exe
```

可加 `actions/cache` 缓存 `~/.cargo` 与 `windows/src-tauri/target` 缩短冷构建
（全量 release + LTO 冷构建约 10–20 分钟）。
