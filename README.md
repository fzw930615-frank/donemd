# 马上做完（Done.md）

> 一个原生的 Markdown 阅读 / 编辑器 + 飞书桥（a native Markdown reader/editor with a Feishu bridge）——macOS 与 Windows 双端。

<p align="center">
  <img src="https://github.com/user-attachments/assets/9226095a-73d7-4dea-bab4-aa8e68daf38d" width="820" alt="马上做完 · 三栏结构：左侧编辑，右侧 Markdown 源码实时同步，点大纲两栏同步跳转">
</p>

**马上做完**（英文 wordmark：**Done.md**）是给 Mac 和 Windows 用的 Markdown 编辑器。文件永远是一份干净的纯文本 `.md`，在应用里打开却是所见即所得的成品——你不用在「干净」和「好看」之间二选一。

## 它能干嘛（Features）

- **双栏实时镜像**：左边所见即所得地写（Visual 视图），右边实时显示对应的 Markdown 源码——左边改一处，右边立刻跟着变。<sub>（当前编辑在左侧富文本进行，右侧源码为实时镜像。）</sub>
- **文档大纲边栏**：长文档按标题层级导航，点一下标题两栏一起滚到位。
- **飞书桥（Feishu bridge）**：本地 `.md` 与飞书文档双向同步——标题 / 列表 / 表格 / 高亮块 / Mermaid 走标准 Markdown，飞书独有的块（电子表格 / 画板 / 多维表格等）以占位块保留，来回同步不丢内容。<sub>（部分格式与排版细节两侧可能略有差异，但不影响内容同步。）</sub><sub>（Windows 版移植中，见下节。）</sub>
- **AI 助手（AI assistant）**：选中文字浮窗、或 `⌘/`（Windows 为 `Ctrl+/`）唤起，做润色 / 翻译 / 续写 / 生成等轻量转换。支持多个 Provider（DeepSeek / Gemini / OpenAI / Claude / MiMo）。
- **精致渲染（Rich rendering）**：代码块语法高亮 + 一键复制、KaTeX 数学公式、Mermaid 图实时渲染、表格 / 图片样式打磨、图片全屏预览。
- **写作背景主题（Themes）**：跟随系统 / 纸质 / 赛博夜色三套整窗主题，纯显示层、绝不写进磁盘文件。

## 功能演示（In action）

### 🧱 框架：左编辑，右实时同步，大纲一点两栏同跳

左侧所见即所得地编辑，右侧 Markdown 源码实时长出来；打开左侧大纲栏点标题，Visual 与源码两栏同时滚到位。下面这段完整演示了左侧编辑、右侧源码即时更新、以及大纲导航：

https://github.com/user-attachments/assets/ab6b3d57-90a9-4094-a8a7-8bd0bb84bf1e

<table>
  <tr>
    <td><img src="https://github.com/user-attachments/assets/5d2fd1ea-ac2b-4891-90ed-6776a0ebe53b" alt="大纲收起：Visual 编辑 + Markdown 源码双栏"></td>
    <td><img src="https://github.com/user-attachments/assets/eb476216-b5f3-4df4-a6dd-7d9c0ed6eed5" alt="大纲展开：三栏结构，左侧文档大纲导航"></td>
  </tr>
  <tr>
    <td align="center"><sub>大纲收起 · 双栏</sub></td>
    <td align="center"><sub>大纲展开 · 三栏</sub></td>
  </tr>
</table>

### 🎨 渲染质量：代码 / 公式 / 图表 / 表格都好看

highlight.js 代码高亮 + 一键复制、KaTeX 数学公式、Mermaid 图表、圆角表格、图片点开系统全屏预览。下面这段完整走了一遍各类格式的渲染效果：

https://github.com/user-attachments/assets/4bed19d9-a95e-4bfc-af86-090f86c279a9

<table>
  <tr>
    <td><img src="https://github.com/user-attachments/assets/a222f4cf-2840-470f-94c3-bbe7a37a7823" alt="渲染格式展示"></td>
    <td><img src="https://github.com/user-attachments/assets/ece7f046-9bc8-465e-a0e3-cc1559707265" alt="渲染格式展示"></td>
  </tr>
  <tr>
    <td><img src="https://github.com/user-attachments/assets/86f80c42-d55b-4e82-92f0-355d1a073ba1" alt="渲染格式展示"></td>
    <td><img src="https://github.com/user-attachments/assets/f47ac422-50d0-484a-a9c0-a53644cf2dd4" alt="渲染格式展示"></td>
  </tr>
</table>

### 🎨 写作背景主题：跟随系统 / 纸质 / 赛博夜色

三套整窗主题——背景、强调色、callout、代码块、Mermaid 全套按主题染色。纯显示层，绝不写进磁盘文件，导出与飞书同步都不受影响。

https://github.com/user-attachments/assets/43c13ba2-a022-45c3-b4d4-2af0d8140909

<table>
  <tr>
    <td><img src="https://github.com/user-attachments/assets/a67e5b4e-9704-4eca-8dd2-5b09e07a98fc" alt="跟随系统主题"></td>
    <td><img src="https://github.com/user-attachments/assets/a6b24b87-c427-42c6-b74e-4723ce452a33" alt="跟随系统主题"></td>
  </tr>
  <tr><td colspan="2" align="center"><sub>🖥️ 跟随系统（深 / 浅）</sub></td></tr>
  <tr>
    <td><img src="https://github.com/user-attachments/assets/5ced4058-01de-4814-bb76-68481340bcf7" alt="纸质主题"></td>
    <td><img src="https://github.com/user-attachments/assets/32a57bb9-5103-4e18-a060-0cbacb617c80" alt="纸质主题"></td>
  </tr>
  <tr><td colspan="2" align="center"><sub>📜 纸质</sub></td></tr>
  <tr>
    <td><img src="https://github.com/user-attachments/assets/004c31e0-4ebd-4d5e-81a5-388deeed639a" alt="赛博夜色主题"></td>
    <td><img src="https://github.com/user-attachments/assets/f017ff30-8152-4298-b0dc-1ebddd54e98b" alt="赛博夜色主题"></td>
  </tr>
  <tr><td colspan="2" align="center"><sub>🌃 赛博夜色</sub></td></tr>
</table>

### 🤖 AI 助手：选中即用

选中文字弹浮窗，或按 `⌘/`（Windows 为 `Ctrl+/`）唤起：润色 / 翻译 / 续写 / 生成，流式替换 + 内联 diff 一键接受或撤销。支持多个 Provider（DeepSeek / Gemini / OpenAI / Claude / MiMo），当前模型在右上角标出。

https://github.com/user-attachments/assets/7b64b55d-175b-48ba-a5ab-36ddf32289c5

<table>
  <tr>
    <td><img src="docs/readme-assets/ai-panel-trigger.png" alt="空行快捷键唤起 AI 面板"></td>
    <td><img src="docs/readme-assets/ai-streaming.png" alt="AI 实时文字流"></td>
    <td><img src="docs/readme-assets/ai-diff.png" alt="红绿修改 diff 可视化"></td>
  </tr>
  <tr>
    <td align="center"><sub>空行快捷键唤起面板</sub></td>
    <td align="center"><sub>AI 实时文字流</sub></td>
    <td align="center"><sub>红绿修改 diff 可视化</sub></td>
  </tr>
</table>

<table>
  <tr>
    <td><img src="docs/readme-assets/ai-settings-model.png" alt="设置里选择 AI 模型"></td>
    <td><img src="docs/readme-assets/ai-current-model.png" alt="右上角高亮当前 AI 模型"></td>
  </tr>
  <tr>
    <td align="center"><sub>设置里选择模型</sub></td>
    <td align="center"><sub>右上角高亮当前模型</sub></td>
  </tr>
</table>

### 🔗 飞书同步：一键推送

`⌘⌥S` 把当前文档同步进飞书，块结构（标题 / 表格 / callout / 图片）保真；`⌘⌥O` 从飞书拉回本地。标准 Markdown 双向无损，飞书独有块以占位保留。<sub>（目前为 macOS 版功能，Windows 版移植中。）</sub>

https://github.com/user-attachments/assets/22e79c48-d9c7-476a-9af8-1c6561a1447c

<p align="center">
  <img src="https://github.com/user-attachments/assets/7d17a2b0-5f5b-41f3-9db4-a86ca45ec224" width="820" alt="飞书同步：本地文档一键推送到飞书文档">
</p>

## 设计原则（Design principles）

- **单文件编辑器 + 桥**：一份文档 = 一个 `.md` 文件 = 一个窗口。不做笔记库管理、不接管你的文件系统、不建索引。
- **稳定规范化（Stable normalization）**：保存产出确定性的标准格式，之后同一状态永远产出同样字节——git diff 永远干净。
- **不做 RAG**：应用只是阅读 / 编辑器 + 飞书桥。你的 AI / 检索工作流直接读文件系统，应用不当中间人。

## 隐私（Privacy）

**马上做完是一个纯本地桌面应用（macOS + Windows），由我一个人独立开发，没有任何后端服务器。** 你的文档、凭据、AI 请求都不会发给我——我这边也没有任何能收集它们的地方。

- **AI 功能的 API key 只存在你本机**（macOS 钥匙串 Keychain / Windows 凭据管理器 CredMan，均为系统级加密存储）。它**不会上传到任何服务器、也不经过我手里**——AI 请求从你的设备**直连你选择的服务商**（DeepSeek / Gemini / OpenAI / Claude / MiMo），用的是**你自己账号申请的 key**。
- **飞书用的是你自己的账号授权**：通过飞书官方 OAuth 在系统浏览器里登录，同步请求直连飞书官方接口。
- **飞书的 App ID、App Secret 和登录凭据同样只存你本机**（macOS 钥匙串 / Windows 凭据管理器），不上传、不经手。<sub>（Windows 侧已落地应用凭证与登录凭据的系统级加密存储；推拉同步仍在移植中，见下节。）</sub>
- **文件永远只在你的磁盘上**：一份文档就是一个本地 `.md` 文件，应用不建云端副本、不建索引、不做云同步。

## 系统要求（Requirements）

- macOS 13+（Ventura 或更新）
- Windows 10/11 x64（WebView2 Runtime：Win11 预装，Win10 一般也有，缺失时安装包会引导）

## Windows 版

Windows 版基于 **Tauri 2**：同一份 `web/` 编辑器产物 + Rust 原生壳，两端通过统一的版本化信封协议通信（契约见 `tasks/bridge-contract.md`），`.md` 落盘格式与 Mac 版**字节兼容**——同一份文档两个平台来回编辑，git diff 依然干净。

界面与功能对齐 Mac：左侧所见即所得编辑 + 右侧 Markdown 源码实时镜像（含 AI 可读性读数）、文档大纲边栏、三套整窗主题、代码高亮 / KaTeX / Mermaid / 表格渲染、AI 助手（5 Provider 流式 + 内联 diff）。快捷键把 Mac 的 `⌘`/`⌥` 映射为 `Ctrl`/`Alt`。

进度按里程碑推进，逐项验收（自动测试 + 真机实测），详见 `tasks/todo.md`：

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M0 | Web ↔ 原生桥接契约抽取 | ✅ 完成 |
| M1 | Tauri 壳、菜单、新建/打开/保存/另存、脏关闭确认、文件关联 | ✅ 完成 |
| M2 | Markdown 引擎移植（Swift 2100 行 → Rust，含 frontmatter / 数学 / 表格 / callout / 占位块） | ✅ 完成 |
| M3 | AI 助手（5 Provider SSE 流、Windows 凭据管理器存 key、设置窗、`Ctrl+/` 唤起） | ✅ 完成 |
| M3.5 | 大纲边栏（侧栏 child webview、双向中继、scrollspy 高亮、`Ctrl+Shift+O`） | ✅ 完成 |
| M3.6 | 双栏实时镜像（右侧 Markdown 源码栏，含标题字号分层与 token 读数） | ✅ 完成 |
| M4 | 资源协议强化（图片读写去重、视频 HTTP Range seek 有界读取） | ✅ 完成 |
| M5 | MSI/NSIS 安装包、CI 说明 | ✅ 完成 |
| M6 | macOS 构建链路回归 | ✅ 完成 |
| M7 | 全新机器综合验收 | ✅ 完成 |
| M8 | 飞书同步移植（Swift → Rust） | 🔄 移植中：转换层、网络与身份、凭据 / 登录 / 设置页已落地；推拉同步引擎未接入 |

当前测试面（2026-09-22）：Rust `cargo test` **360 通过**、Web `npm test` **58 通过**（含 Swift 侧测试移植）。

Windows 版与 Mac 版的已知差异：

- **飞书同步尚未接入**（M8 移植中）——Windows 侧只能配置应用凭证与登录飞书，推拉文档暂不可用。设置窗内已可见「飞书同步」分段。
- **源码栏不显示 frontmatter**（Mac 顶栏有显示开关，Windows 首版不做）。
- **快捷键按 Windows 惯例落地**：标题级别用 `Ctrl+Alt+0..6`（Mac 为 `⌘⌥0..6`），大纲开关用 `Ctrl+Shift+O`（Mac 为 `⌃⌘S`，`⌃` 在 Windows 无对应修饰键、`Ctrl+Shift+S` 已是另存为）。
- 其余功能（编辑 / 渲染 / 主题 / AI / 图片视频）与 Mac 一致，同一份 `web/` 产物。

Windows 端构建：

```bash
cd windows
npm install
npm run dev          # 开发模式（热加载）
npm run build-debug  # 调试构建：src-tauri/target/debug/donemd.exe
npm run build        # 发布构建 + MSI/NSIS 安装包
```

> 注意：不要直接 `cargo build` 后运行 debug exe——调试配置默认指向 vite 开发服务器（localhost:5173），没起 `npm run dev` 时窗口会显示「无法访问」。要独立运行的调试包请用 `npm run build-debug`（静态产物已打进二进制）。

安装包与构建细节（产物路径、版本号策略、代码签名、CI）见 [`BUILD.md`](BUILD.md) 的 Windows 章节。

## 安装（Install）

### macOS

> ⚠️ **当前是未公证的预览版（preview）**——功能完整，但签名 + Apple 公证的正式版还在路上，所以第一次打开需要手动放行一次（下面第 3 步）。

1. 到 [Releases](https://github.com/shampooli61/donemd/releases) 下载最新的预览版 `Done.md-x.y.z.dmg`
2. 双击 `.dmg`，把 `donemd.app` 拖进「应用程序」
3. **第一次打开**（只需一次，之后跟普通应用一样）——三选一：
   - **命令行最省事**：终端运行 `xattr -dr com.apple.quarantine /Applications/donemd.app`，之后双击即开
   - **系统设置**（macOS 15 / 26）：双击 → 弹「无法验证」点「完成」→ 打开「系统设置 → 隐私与安全性」→ 滚到底点「仍要打开」→ 再确认一次
   - **右键打开**（macOS 13 / 14）：在「应用程序」里右键 `donemd.app` →「打开」→ 再点「打开」按钮

> 正式的签名公证版发布后，首次打开就不再需要这一步。

### Windows

Windows 安装包（WiX MSI 与 NSIS `setup.exe`）由本仓库的 `npm run build` 产出，产物路径见 [`BUILD.md`](BUILD.md)：

- `windows/src-tauri/target/release/bundle/msi/Done.md_<version>_x64_en-US.msi`
- `windows/src-tauri/target/release/bundle/nsis/Done.md_<version>_x64-setup.exe`

任选其一安装即可（两个包都会注册 `.md` / `.markdown` 文件关联，装完双击 .md 直接进 Done.md）。

> ⚠️ **当前未做代码签名**——首次安装/运行会触发 SmartScreen「Windows 已保护你的电脑」，点「更多信息 → 仍要运行」即可。正式发布前再评估 EV 证书或 Azure Trusted Signing。

## 从源码构建（Build from source）

见 [`BUILD.md`](BUILD.md)——macOS 侧含前提条件、首次拉取、`project.yml` → xcodegen 流程、命令行 build & test；Windows 侧（Tauri 2）含 Rust/Node 前提、日常开发与调试包、`cargo test`/`vitest`、发布打包与 CI。

## 技术栈（Tech stack）

- **外壳**：macOS 为 SwiftUI App；Windows 为 Tauri 2（Rust）+ WebView2
- **编辑器内核**：WKWebView / WebView2 内嵌 Tiptap (ProseMirror) + CodeMirror，双向同步（两端复用同一份 `web/` 构建产物与信封协议）
- **渲染**：Mermaid（图）/ KaTeX（公式）/ highlight.js（代码）
- **飞书**：原生自建（macOS `donemd/Feishu/`、Windows `windows/src-tauri/src/feishu/`）——走飞书官方 OAuth + 开放接口，块级结构化双向同步

## 命名（Naming）

品牌显示名是「马上做完（Done.md）」；技术标识符统一用 `donemd`（仓库 / 目录 / 命令行），Bundle ID 为 `com.shampoo.donemd`。
