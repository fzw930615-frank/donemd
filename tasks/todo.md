# Windows 构建支持计划

## 路线（已确认）

**Tauri 2**（2026-09-16 确认）：复用 `web/` 编辑器产物，Rust 原生壳经统一信封协议桥接；
输出 Windows `.msi`/`.exe`。桥接契约见 [`bridge-contract.md`](bridge-contract.md)。

- 支持范围：Windows 10/11 x64（ARM64 待打包阶段验证）；macOS 版保留且构建链路不受影响。

## 验收流程约定

每个里程碑按固定顺序验收，全部通过才勾选：

1. **自动验收**：`cargo check` 零错误 → `cargo test` 全绿 → 构建成功（命令见各节）。
2. **人工验收**：按里程碑的实测清单逐项验证（需要真机窗口的标 🖐️）。
3. 在本文对应条目打勾并写明日期；未通过项记录现象与处置。

## 里程碑

### M0 桥接契约抽取 — ✅ 2026-09-16

- `tasks/bridge-contract.md`：信封格式、15 种 JS→原生、14 种原生→JS、`donemd-asset://` 资源协议、Swift→Rust 移植清单。
- 验收：契约与 `WebViewBridge.swift` 实际消息面逐项对齐。

### M1 Tauri 壳 + 文档生命周期 — ✅（自动验收）/ 🖐️（人工待测）

- `windows/` 脚手架（Tauri 2 + `tauri-plugin-dialog`）；`web/src/bridge.ts` 增加 Tauri 传输层（信封零改动，macOS 路径不变）。
- 原生菜单（文件/格式/插入）、新建/打开/保存/另存（原子写入）、脏关闭三键确认、文件关联启动（argv）。
- 保存握手：Tauri 无 evaluateJavaScript，走 `requestDocumentJSON` → `documentJSON` 回环（`state.pending_save` 防重入）。
- 自动验收（2026-09-16 全过）：
  - `cargo check` 零错误（修复 11 处：menu.rs 泛型 Runtime → 具体 Wry 类型 + `Manager` 导入；main.rs `app.handle()`；`Menu<tauri::Wry>` 显式泛型）。
  - `web/build.mjs` 替换 POSIX 内联环境变量语法（cmd.exe 不支持），`npm run build` 现跨平台；macOS 侧需在 M6 回归。
  - `tauri build --debug --no-bundle` 产出 `windows/src-tauri/target/debug/donemd.exe`（约 17 MB）。
  - 冒烟：启动后 6 秒进程存活。
- 🖐️ 人工验收清单（待实测）：启动开窗 → 编辑标脏（标题栏 `•`）→ Ctrl+S 保存 → 关闭弹三键框 → 重开文件内容一致 → 双击 .md 关联启动。

### M2 Markdown 引擎移植 — ✅ 2026-09-16

- Swift 约 2100 行 → Rust 约 2200 行（`markdown/`：ast / convert / serializer / frontmatter / tiptap / placeholder），pulldown-cmark 替代 swift-markdown。
- 自动验收：`cargo test` **29/29**。用例含 macOS 测试移植——规范化 6 项（setext→ATX、`*`/`+`→`-`、空行折叠、末尾单换行、`_`→`*`）、frontmatter 边界 6 项（TOML 拒绝、未闭合回退、坏 YAML 回退、非文件头、空围栏、用户字段逐字）、固定点 fixture（嵌套列表、多段 callout、HTML details、硬换行、多行数学块、飞书占位块、表格、任务列表等）。
- 已知差异（与 macOS 行为一致，非回归）：frontmatter 后正文的首个空行在全文档往返时被规范化掉；Swift 侧同样只保证 frontmatter 等价（`testMarkdownEngineParsedDocumentRoundTripsFrontmatter` 只断言 frontmatter 相等）。原始 body 层往返（`frontmatter::parse`→`serialize`）字节精确。

### M3 AI 功能 — ✅（自动验收 2026-09-16）/ 🖐️（人工待测）

- 移植面：Swift `AI/` 目录 → `windows/src-tauri/src/ai/`（`prompt.rs` 16 命令指令与上下文组装、`providers.rs` 5 Provider/3 协议族 + JSON 配置、`clients.rs` OpenAI 兼容/Anthropic/Gemini 三族 SSE 解析与请求构造、`credentials.rs` Windows 凭据管理器替代 Keychain、`mod.rs` StreamCoordinator 等价物）。
- 语义对齐：单流并发（忙碌回 `aiStreamBusy`）、60s 超时、ESC 静默取消（`tokio::select!` + `Notify`）、重试原样重放 `last_request`、8K 字符预算降级 `aiStreamDegrade`、转表格「不适合」检测、错误文案逐条对齐（含 canOpenSettings 标记）。
- 续写走 `requestDocumentJSON {requestId: "ai-doc-<uuid>"}` 握手取全文（与保存共用通道，`document.rs` 按前缀路由）。
- 配置/凭据：`%APPDATA%\com.shampoo.donemd\ai-config.json`（原子写入）+ Windows 凭据管理器（服务名 `com.shampoo.donemd.<provider>`）；key 永不落盘明文、也不回传 webview（设置窗不回填已存 key，空字段保存时复用已存 key——便于只改 endpoint）。
- 设置窗：`web/settings.html` + `src/settings.ts`（第三构建入口），独立 Tauri 窗口（label `settings`，无菜单栏、随系统主题画布），经 `ai_settings_load/save_key/clear_key/refresh_models/update` 命令读写；保存即「测试连接 + 拉模型列表」，失败降级默认模型不阻塞保存。菜单 文件 → AI 设置…（Ctrl+,）+ 错误 toast「打开 Provider 设置」双入口。
- 唤起：`Ctrl+/` 无需改动（`slash-menu.ts` 本就同时认 metaKey/ctrlKey）。
- 自动验收（2026-09-16 全过）：`cargo check` 零错误；`cargo test` **60/60**（29 → 60，新增 prompt 组装 11、配置 5、SSE 解析与请求构造 12、降级/错误文案 2 等）；`cargo build` 链接通过；`npm run build` 三入口产出 `settings.html`（9.7 KB）。
- 修复：关闭确认原先对所有窗口生效（设置窗关闭会误触文档保存流程）——已按窗口 label 限定 main。
- 🖐️ 人工验收清单（待实测）：AI 设置窗存 key + 拉模型列表 → Ctrl+/ 润色/翻译/续写流式替换 → ESC 取消 → 失败后重试 → 未配置时错误 toast 跳转设置窗。
- 人工实测修复 2 项（2026-09-21）：① **模型下拉 8+ 项显示不全**——`createSelect` 弹层原为 `position:absolute`，被 Provider 卡片 `.card { overflow:hidden }`（圆角裁剪）截断，卡片外的选项不可达；改为 `position:fixed` 逃逸祖先裁剪 + 新纯函数 `web/src/select-pop-layout.ts` 视口感知布局（下方空间不足 120px 向上翻、maxHeight 钳到可用空间 60–240px、人脸矩形先钳到视口内防滚动中途点击算出负坐标），打开时把当前模型滚到列表中部；页滚/resize 关闭、弹层内部滚动不关。vitest 新增 10 例（48 → 58）。② **`aiOpenSettings` 走桥接信封死锁整个应用**——错误 toast「打开 Provider 设置」按钮 invoke `bridge_dispatch` 在主线程的 WebView2 回调里同步 `WebviewWindowBuilder::build()`，嵌套的控制器创建永远完不成：窗口卡 about:blank、invoke 永不返回、主窗冻结（菜单入口走事件循环分发不嵌套所以侥幸能用）；`open_settings` 的建窗挪到 `std::thread::spawn`，创建请求走事件循环代理即可。CDP 实测：12 模型 + 人脸贴视口底最坏情况下弹层向上翻、fitsViewport、可滚动、第 12 项可达、页滚关闭/内滚保持。
- 待办：M6 macOS 回归需含 `web/build.mjs` 三入口（新增 settings）。

### M3.5 大纲边栏（Windows）— ✅（自动验收 2026-09-20）/ 🖐️（人工待测）

- macOS 侧是 SwiftUI `OutlineSidebar`（NavigationSplitView 列）；Windows 无等价物，改为**主窗内左侧 child webview**（Tauri 2 多 webview，宽 260px），复用 `web/` 构建产物。
- ✅ Web 侧栏页面：`web/outline.html` + `src/outline.ts` + `src/outline.css`——纯投影：收 `outlineSet {headings}` / `outlineActive {index|null}`，发 `outlineJump {index}` / `editorReady`；scrollspy 高亮 + 点击乐观选中（对齐 macOS pendingIndex 语义）。`web/build.mjs` 四入口含 `outline`。
- ✅ 数据源现成：`main.ts` 的 `outlineChanged`（300ms 防抖）与 `scrollToHeading`（heading-ordinal 锚定）均为 Mac 遗产，Windows 直接复用；`activeHeadingChanged`（scrollspy）同。
- ✅ 壳侧接线（2026-09-20 完成）：
  1. `outline.rs`（新模块）：`Window::add_child` 创建/销毁侧栏、主 webview `set_bounds` 右移、窗口 `Resized` 联动重排、随系统主题刷底色（页面 CSS 透明契约同 settings）。侧栏停靠期间主 webview `set_auto_resize(false)`，防止窗口 resize 时主编辑器回弹盖住侧栏。
  2. `bridge_dispatch` 新增发送者参数（`webview: Webview` 注入）：`editorReady` 按 label 路由——主窗走原文档加载，侧栏 ready 回 `outlineSet`+`outlineActive` 快照。
  3. 中继：`outlineChanged`/`activeHeadingChanged` 存 state 后分别发 `outlineSet`/`outlineActive` 给侧栏（侧栏隐藏时不发、不报错刷屏）；`outlineJump` → `scrollToHeading` 发主编辑器（源码镜像栏未做，只发 Visual）。
  4. 菜单「视图 → 文档大纲」（CheckMenuItem，`CmdOrCtrl+Shift+O`）+ main.ts JS 兜底（首个真正落地的 `menuCommand` 信封——此前注释声称的 Ctrl+N/O/S 兜底从未实现）。快捷键选型：macOS 是 ⌃⌘S，⌃ 在 Windows 无对应修饰键且 Ctrl+Shift+S 已是另存为，取 Ctrl+Shift+O。
  5. `Cargo.toml` 开 `tauri` 的 **`unstable` feature**——多 webview API（`add_child`/`get_webview`）整体在其下；`Emitter::emit` 在 webview 对象上调用实为全局广播，全部改为 `emit_to(label)` 定向投递。
- 自动验收（2026-09-20 全过）：`cargo check` 零错误（死码警告减 1——`OutlineHeading` 接入消除）；`cargo test` 65/65；`cargo build` 链接通过；`npm run build` 四入口产出 `outline.html`（4.4 KB）；vitest 35/35。
- 🖐️ 人工验收（2026-09-20 部分通过）：开关/布局/联动/重开显示 ✅（修复后）；scrollspy 高亮、主题跟随、新建/打开刷新待逐项过。
- 人工实测修复 2 项：① **侧栏重开不显示**——`Webview::close()` 注销是同步的但 WebView2 销毁走事件队列，同名 label 重建静默失败；改为创建一次、常驻 hide/show（顺带秒开 + 保留滚动位置，对齐 macOS 手感）；`tauri::Webview` 无 `is_visible()` getter，可见性用模块级 `AtomicBool` 自维护。② **文档已打开时首开侧栏黑屏**——`tauri.event.listen('bridge')` 的 JS 侧注册是异步往返，4.4 KB 的 outline 页面模块执行完就发 `editorReady`，快照回发时监听器没就位被丢弃（空文档时丢空快照不可见，故 Case B 掩盖）；修复为 `bridge.ts` 导出 `bridgeReady` Promise，两页面都等监听就绪再发 `editorReady`（visual 页 4.6 MB 一直侥幸没踩过，同为隐患一并修）。
- 已知偏差：Windows 首版无右侧源码镜像栏（M3.6 排入），`outlineJump` 只发 Visual 栏（outline.ts 注释中的「双栏同步滚动」为 macOS 语义保留）。

### M3.6 双栏实时镜像（Windows 源码栏）— ✅（自动验收 2026-09-20）/ 🖐️（人工待测）

- Mac 为「左 Visual + 右 Markdown 源码」常驻双栏（无开关）；Windows 对齐：启动即在主窗右侧 `add_child` 源码 child webview（`markdown-source.html`，只读镜像），大纲栏显示时三栏 `[outline 260px][visual][source]`（visual/source 平分剩余宽度）。
- 同步流（替代 macOS 的 evaluateJavaScript 拉取）：Visual `documentChanged`（rAF ~60Hz）→ 原生回 `requestDocumentJSON {requestId:"source-sync-<epoch>"}` → `documentJSON` 回包 → Rust `markdown::serialize` → `setMarkdownSource {text}` 推源码栏。`state.source_sync_pending` 防重入：在途期间的新变更跳过（回包携带回复瞬间的最新 doc，不丢更新）；`<epoch>` 后缀防新建/打开途中旧文档的回包落地（`document.rs::route_source_sync` 吞掉过期 epoch）。
- 双栏改造点：`editorReady` 路由加第三臂 `source`（回序列化快照 + 折叠集合）；`applyFold`/`scrollToHeading` 由只发 main 改为 fan-out main+source；`badMathFormulas` 由空转臂改为中继源码栏；`document.rs` 打开/新建后主动推一次 `setMarkdownSource`（覆盖源码栏已加载场景，加载中场景由 editorReady 快照兜底）；`markdown-source.ts` 的 `editorReady` 同样走 `bridgeReady` 门（M3.5 竞态修复同源）。
- 新模块：`layout.rs` 为唯一布局真源（大纲显隐 + 窗口 resize 都在此重排三栏；主 webview 常驻 `set_auto_resize(false)`，0×0 最小化尺寸忽略）；`source.rs` 启动即建常驻源码栏（macOS 对齐：无开关）；`theme.rs` 主题翻页时三栏 canvas 一并重刷。
- 标题体系三件套（2026-09-21）：① 快捷键由 Mod-Shift-1..3 改为 **Mod-Alt-1..6**（Windows 即 Ctrl+Alt+1..6，Word/Pages 惯例，也正好是 Tiptap 默认值——覆盖扩展继续保留只为表格单元格守卫 #76）；Mod-Alt-0 正文已有。② 格式菜单新增标题组：正文 + 一级…六级标题，显示 Ctrl+Alt+0..6 快捷键（`fmt:paragraph`/`fmt:headingN` 走既有 formatCommand 通道）。③ 光标气泡菜单头部新增「标题级别」下拉：按钮回显光标所在块级别（正文/H1…H6），点击在近光标处弹 7 行选单（名称 + 快捷键提示 + 当前行高亮），选中即应用；与 AI 下拉互斥开合。动作并入 `runFormatCommand` 单一真源（菜单/气泡共用，表格单元格内拒转标题同键盘路径）。顺带把气泡按钮 tooltip 的硬编码 "Cmd+…" 全部改走 `MOD`/`ALT` 常量（bridge.ts 新增 `ALT`：Win=Alt+ / Mac=⌥）。CDP 实测：Ctrl+Alt+2/4/0 逐级切换与回归正文 ✅，气泡弹层 7 行带 Ctrl+Alt+N 提示、当前行高亮、点击应用并回显 H3 ▾ ✅。
- 标题字号分层（2026-09-21）：源码栏 h1–h6 整行字号阶梯（1.55/1.36/1.2/1.08/0.98/0.9em on 13.5px 正文字号，h4+ 字重降 600，对齐 Visual 的层级比例但压扁——仍是源码视图）。`markdown-source.ts` 新增 `headingLineField`（StateField 行装饰，复用 fold 同款 fence-aware `scanHeadings`，代码块内 `#` 行不放大）；CSS 加 `cmd-md-h1..6`。CDP 实测六级 20.9/18.4/16.2/14.6/13.2/12.2px 严格递减、代码块内不生效。
- 顶栏读数（#82 移植，2026-09-21）：Mac 源码栏顶栏的 AI 可读性读数移植为源码页内 28px 顶条（Windows 壳无原生 SwiftUI 条，读数在页内从本栏已收到的 markdown 直接算，无原生往返）。`web/src/doc-size.ts` 为 `DocumentSizeEstimate.swift` 的 TS 移植（CJK 0.6 token/字、其他 1/4，≤16K 🟢 / 16–64K 🟡 / >64K 🔴，同套阈值与文案），补 `charactersReadable`；`markdown-source.html/css/ts` 加顶条（色点 + ≈tokens + · + 大白话结论，体积/字数进 tooltip，四调色板各配 `--cmd-secondary/tertiary`），`setMarkdownSource` 末尾刷新读数。首行对齐：`.cm-content` padding-top 20→16px，顶条 28+16=44px 与 Visual 首行齐平。左侧 frontmatter 开关首版仍不移植。
- 已知差异：frontmatter 不显示在源码栏（Mac 顶栏有显示开关，Windows 首版不做）；源码栏无独立菜单开关（与 Mac 一致）。
- 人工实测修复 1 项（2026-09-21）：**源码栏只显示几个折叠箭头、文字不可见**——Tauri 资产 CSP 改写会往 `style-src` 追加每次加载随机的 `'nonce-…'`，按 CSP 规范 nonce 一旦存在 `'unsafe-inline'` 即被忽略，CodeMirror style-mod 运行时注入的 `<style>` 标签（无 nonce）被整体拦截，CM6 基础布局样式（scroller flex 等）全丢：gutters 以块级堆在 content 上方把它顶出视口。修复：`tauri.conf.json` 开 `dangerousDisableAssetCspModification: true`，CSP 按书写生效（`style-src 'unsafe-inline'` 本来就是声明意图）。经 CDP 验证修复后 scroller flex、content y=0、无双栏 CSP 违规；顺带覆盖一切运行时样式注入类库（mermaid 等）。
- 自动验收（2026-09-20 全过）：`cargo check` 零错误（4 个预留死码警告不变）；`cargo test` 65/65；`npm run build` 四入口产出 `markdown-source.html`（603 KB，CodeMirror 占大头）；vitest 35/35；`npm run build-debug` 链接通过。冒烟：启动 8 秒进程存活，stderr 确认双栏各半（1200×780 逻辑像素下 visual/source 各 600）、source 栏 `editorReady` 先于 main 到达且快照正常回发。
- 🖐️ 人工验收清单（待实测）：启动即见双栏 → 左侧打字右侧 ~1 帧内跟随 → 滚动位置保持 → 大纲点行两栏同步滚动 → 折叠 chevron 两栏互同步 → 坏公式源码栏标红 → 三栏（开大纲）resize 稳定。

### M4 资源协议强化 — ✅（自动验收 + CDP 实测 2026-09-21）/ 🖐️（人工待测）

- `donemd-asset://` 已可读写图片（sha256 去重、未保存文档走临时目录、首存迁移）。
- HTTP Range（#88 / ADR-0009 对齐，2026-09-21）：单区间 `bytes=START-END` / `START-` / `-SUFFIX`，206 + `Content-Range`/`Accept-Ranges`；畸形、multipart、不满足（start≥total）、空文件一律回落 200 全量。**修复性能缺陷**：首版先 `fs::read` 整文件再切片，大视频每次 seek 都全量入内存；现对齐 Mac 的 `FileHandle.seek+readData(ofLength:)`——`metadata` 取总长，`File::open + seek + take` 有界读取（`read_range`）。
- 测试移植（`assets.rs` 新增 `#[cfg(test)]`，65 → **98**）：`AssetURLSchemeHandlerTests.swift` 的 12 例 Range 解析 + MIME 全表 + URI 文件名提取（plain/前导斜杠/query/fragment/百分号解码）；`AssetsManagerTests.swift` 的导入落盘/相同字节去重且 mtime 不变/按需建目录/未标题写暂存/视频扩展名/`stored_file_path` 暂存回退/首存迁移三态；另补响应层 5 例（200 全量带 Accept-Ranges、206 闭区间/开口/后缀切片精确、不满足回落 200、空文件回落）。顺带补 `extension_for_mime("image/jpg")` 对齐 Mac。
- 结构：`handle_asset_request` 拆为薄壳（URI→文件名→`stored_file_path`）+ `respond_with_file`（可测的纯响应构造）+ `asset_filename_from_uri`。
- CDP 实测（2026-09-21，10.7 MB H.264 30s 真实视频，`tasks/m4-video-test/` + `probe-range.mjs`）：`<video>` 经协议加载 `readyState=4`、duration=30、`seekable=[0,30]`；JS 置 `currentTime=18` 触发 `seeked` 精确落点；Network 域抓到 seek 请求回 **206** `Content-Range: bytes 5963776-10735944/10735945`、`Accept-Ranges: bytes`、`video/mp4`——WebView2 媒体栈 → Tauri 协议 → seek 有界读取全链路通。注：页面 CSP `connect-src` 不放行资产域，fetch 探针不可行，改走 Network 域 + 真实 `<video>`（更贴近用户路径）。
- 🖐️ 人工验收清单（待实测）：大视频（百 MB 级）拖动进度条流畅度、图片系统预览（`open::that_detached` 已接线 bridge.rs 五处：图片预览/链接/本地文件/裸域名/资源管理器揭示）。

### M5 打包与发布 — ✅（自动验收 2026-09-21）/ 🖐️（实装待测）

- `tauri.conf.json` 已配 `msi`+`nsis` 双目标、图标全套、.md 文件关联注册。
- ✅ release 构建 + 打包一次通过（2026-09-21）：`npm run build`（= `tauri build`）→ web 四页构建 → cargo release（lto/codegen-units=1/strip/panic=abort）→ WiX+NSIS 双包。产物：`target/release/bundle/msi/Done.md_1.0.0_x64_en-US.msi`（4.36 MB）、`nsis/Done.md_1.0.0_x64-setup.exe`（3.49 MB）、release exe 本体 7.59 MB（debug 17 MB → 7.6 MB）。首次打包自动下载 NSIS 工具链成功。
- ✅ release exe 冒烟：启动 6 秒进程存活（35 MB 内存）。
- ✅ 版本号策略：Windows `tauri.conf.json.version` 与 Mac `project.yml MARKETING_VERSION` 共用同一营销版本号（当前 1.0.0），发版两处同改；MSI 四段版本由 Tauri 自动派生。已写入 BUILD.md。
- ✅ CI（windows-latest）构建说明补进 BUILD.md：镜像自带 Rust/Node/WebView2/VS Build Tools，骨架 YAML（checkout → setup-node → rust-toolchain → 双端测试 → build → 传产物）+ 缓存建议（全量 release 冷构建 10–20 分钟）。
- ✅ BUILD.md 新增「Windows 构建」章节：前提条件、dev/build-debug 区别（cargo build 直出 exe 指向 devUrl 的坑）、发布构建命令与产物路径、未签名 SmartScreen 说明。
- 🖐️ 人工验收（待实测）：双击 MSI 或 NSIS 安装 → 开始菜单/桌面入口 → 双击 .md 关联启动 → 卸载干净。预期 SmartScreen 提示（未签名），点「更多信息 → 仍要运行」。
  - `msiexec /i "windows\src-tauri\target\release\bundle\msi\Done.md_1.0.0_x64_en-US.msi"` 或直接双击 `nsis\Done.md_1.0.0_x64-setup.exe`。

### M6 macOS 回归 — ✅ 2026-09-22

- 受影响面：`web/package.json` build 脚本改为 `node build.mjs`（四入口）；`web/src/bridge.ts` 增加 Tauri 分支（浏览器/wkwebview 路径不变）。
- 验收：macOS 上 `npm run build` 四入口产出不变 + xcodebuild 构建通过 + `xcodebuild test` 全绿。

### M7 实机综合验收 — ✅ 2026-09-22

- 全新 Windows 机器安装 MSI → 启动 → 打开/编辑/保存/图片/视频/折叠/大纲/快捷键全链路。

### M8 飞书同步移植（macOS Swift → Tauri）— 🔄 F2 完成（2026-09-22）

macOS 原版 `donemd/Feishu/`（24 文件 ~8,800 行 Swift + ~4,300 行测试）逐文件 1:1 移植。五阶段：F1 转换层（纯 Rust 零网络）→ F2 网络+身份 → F3 同步引擎 → F4 接线（含真凭据端到端）→ F5 后置可选（url_detector / sync_root / import）。计划全文见 `C:\Users\frank\.claude\plans\linear-prancing-donut.md`；用户决策：同步根目录后置 F5；有真机飞书自建应用凭证，端到端联调纳入 F4 验收。

- ✅ **F1 转换层（2026-09-22）**：`feishu/{block,callout,encoder,converter}.rs` 全量落地。
  - `block.rs` ← `FeishuBlock.swift`（块模型：block_id/parent_id/children + Payload + TextElementStyle + 占位引用）
  - `callout.rs` ← `FeishuCalloutType.swift`（5 类型 ↔ emoji ↔ wire 命名 emoji_id ↔ 背景色 int 1-15 ↔ `light-*` 名，含别名调色板）
  - `encoder.rs` ← `FeishuBlockEncoder.swift`（wire 编解码：顶层剥 parent_id / 空 elements 合成空 textRun / 真实 wire 型号 view=33·file=23·iframe=26 等 / revision 双容忍）
  - `converter.rs` ← `FeishuStructuralConverter.swift`（blocks ↔ Tiptap 双向：siblings 分组 / quote 容器 / callout 禁入子块过滤 / video 吸收 file 子块 / 占位丢子块警告 / 表格单元格首文本块 + 聚合警告 / EmissionContext `blk_%06d` / 页标题 ↔ 前置 H1 绑定）
  - `markdown/frontmatter.rs` merge 激活（测试接入，F3 接线）；`markdown/placeholder.rs` 测试缺口补齐。
  - 测试 136 → **228 全绿**（+51 converter、+11 frontmatter feishu/merge、+29 placeholder、+1 callout）；`cargo check --tests` 死码警告从 4 → 2（`frontmatter::merge` 消除，剩 `providers.rs path` / `tiptap::set_attrs` 均为既有项）；`cargo build` 通过。
  - 注：`cargo check`（bin 目标）另有 ~87 条 feishu 死码警告为预期过渡墙（模块在 F2/F3 接线前不被二进制消费，`feishu/mod.rs` 头注已记录）。
- ✅ **F2 网络+身份**（F2.1–F2.3 完成，2026-09-22）：
  - ✅ F2.1 `api.rs` ← `FeishuAPIClient.swift` 前半：`FeishuApi` trait（10 方法 AFIT）+ `FeishuApiError`（9 桶）+ `FeishuTransport`/`AccessTokenProvider`/`BackoffSleeper` 注入面 + `classify_response`（401/99991663→Unauthorized、99991679→ScopeInsufficient、403/99991664→Forbidden、404→NotFound、429/5xx→Retryable、2xx+code≠0→Fatal）+ 全端点请求构造（分页/batch_delete/descendant/multipart 上传）+ `FeishuBackoffPolicy`（1s→8s ×2 / 4 次 / jitter 0.5）。44 例。
  - ✅ F2.2 网络面五文件：`http_client.rs`（`FeishuHttpApi<T,P,S>` 泛型编排：401 静默刷新一次重试 / 429·5xx 退避至耗尽（429→RateLimited、5xx→ServerError 携末发信封）/ 图片下载旁路管线（无信封，scope 探测独立实现）/ 分页聚合 + revision 单读 / push 先删后建 / 上传 SHA-256 内容缓存；`ReqwestTransport` 生产传输）+ `oauth.rs`（authorize URL / token 扁平响应解析 / refresh 重申 scope / 60s skew；全依赖注入含时钟与 state 生成器）+ `oauth_receiver.rs`（127.0.0.1:18127 手写 TcpListener 回环：纯函数 `handle_callback` 判定 + GET-only / state 校验 / 「登录成功」页；应答分离任务优雅关闭——立即 drop 会触发 Windows RST 丢应答）+ `credentials.rs`/`app_config.rs`（模型 + 存储抽象，keyring 实现在 F2.3）。零新增 crate 依赖；`OAuthCallbackReceiver` 手装箱 future 换 dyn 装配（AFIT 无 dyn 面，不引 async-trait）。
  - 测试 272 → **324 全绿**（+52：http_client 29 例〔编排组 27 + 凭据边界/存储往返/编译期 trait 证明 2〕+ oauth 12 例〔login/refresh/logout 生命周期 + URL/解析形状〕+ oauth_receiver 11 例〔纯函数判定组〕）；另真端口 2 例 `#[ignore]` 手动跑通过（须 `--test-threads=1`，18127 并行互撞）。
  - 过渡面警告：`cargo check --tests` 10 条（2 既有 + 8 条 F3/F4 接线前消费点：`http_client::new`/`oauth::{new, with_overrides, NotConfigured, redirect_uri}`/`ReceiverError::Cancelled`/`api::{update_document_title, resolve_wiki_node}`）；bin 目标 ~175 条 feishu 死码墙 F2.3 接线后降至 147，F3/F4 消除。
- ✅ F2.3 凭据+配置+管理器+设置页（2026-09-22）：
  - `credentials.rs` keyring 实现（`com.shampoo.donemd.feishu-oauth`/default，JSON 整存整取、save 先删后写、NoEntry→None，`#[ignore]` 探针独立服务名）+ `app_config.rs` 三级解析链（keyring `com.shampoo.donemd.feishu-app-config` > env `DONEMD_FEISHU_APP_ID/SECRET/REDIRECT_URI` > `%APPDATA%\com.shampoo.donemd\feishu-config.json`；三字段缺一即整源失效、keyring 读失败不毒化链，16 例对应 FeishuAppConfigTests）+ `oauth.rs` 补 `force_refresh`（401 重试腿无视过期判定）。
  - `manager.rs` ← `Settings/FeishuSyncManager.swift`（同步根目录段=F5 后置）：AuthState 三态（NotConfigured/LoggedOut/LoggedIn{tenant_key}）、启动只读非机密镜像 `feishu-state.json`（auth_state/tenant_key/client_id/redirect_uri，secret/token 永不落明文）、refresh_auth_state 实时重算（凭据读失败→LoggedOut 不僵死）、save_app_config（trim → secret 留空=沿用已存 → keyring → 状态重算）、login/logout；`OAuthTokenProvider<T>` 接 FeishuHttpApi 令牌需求（错误映射对齐 bearer() 语义）。Swift 侧此文件无单测（浏览器/Keychain 不可注入），Windows 全依赖可注入补 ~14 例。
  - 接线：`state.rs` AppState 加 `feishu` 字段（全 interior Mutex 可直持）+ `main.rs` 注册 5 命令（`feishu_settings_load/save_app_config/clear_app_config/login/logout`）+ setup `boot()`。
  - 设置窗升级通用「设置」窗：`settings.ts` 顶层分段 tab（AI/飞书同步，飞书态懒加载首碰 keyring）+ 飞书 pane（凭证卡片：App ID / App Secret〔不回填，留空=沿用〕/ 重定向 URL+复制，保存到凭据管理器 + 清除（仅 keyring 源），徽标三态 ✓已配置 / env·文件 / 未配置；账号段：notConfigured 引导 / loggedOut 登录飞书〔在途→「正在等待浏览器授权…」〕/ loggedIn ✓已登录+租户+重新登录+登出，lastError 显错）；菜单「AI 设置…」→「设置…」、窗口标题同步。
  - 测试 324 → **360 全绿**（+36：manager ~14 + app_config 17 + credentials 2 + oauth 补充；`#[ignore]` 5 = 既有 3 + keyring 探针 2）；npm build 四入口通过。
  - 🖐️ **人工验收（真凭据，F2 清单）**：设置→飞书同步存凭证（CredMan 两条目）→ 登录飞书 → 浏览器授权 → 回环页「登录成功」→ 已登录（租户）→ 重启保持 → refresh 静默续期 → 登出清理（条目删、镜像 loggedOut）→ 端口占用中文报错。可与 F4 端到端合并跑。
- ⬜ F3 同步引擎：push/pull 协调器、图片上下传 stage、快照/首拉。
- ⬜ F4 接线：菜单 + 进度条/浮层 UI + feishu-doc 握手 + 🖐️ 真凭据端到端。
- ⬜ F5 后置：url_detector / sync_root / import。

## 预留接口（对应 dead-code 警告，随里程碑接入后消除）

| 位置 | 用途 | 接入里程碑 |
|---|---|---|
| `frontmatter::merge` | 拉取后合并 feishu 子树（F1 测试已消费，bin 目标仍待接线） | M8 F3 |
| `feishu::api` trait 面 | `FeishuApi`/`FeishuTransport`/`BackoffSleeper`/错误变体（F2.1 落地；`FeishuTransport`/`BackoffSleeper`/错误全表已被 F2.2 http_client 52 例消费，`update_document_title`/`resolve_wiki_node` 待 F3） | M8 F3 |
| `feishu::http_client` / `oauth` / `oauth_receiver` / `manager` 面 | `FeishuHttpApi::new`/`OAuthTokenProvider`/`http_api()`（F2.3 设置命令面已接 keyring/镜像；推拉命令未建，bin 仍死码） | M8 F4 |
| `tiptap::set_attrs` | Tiptap 节点属性改写工具 | 飞书同步/双向编辑（待排期） |
| `OutlineHeading` 字段 | 大纲 UI 数据 | M3.5 大纲边栏 |
| `AppState::new` | 无参构造 | 测试/多窗口 |

### M9 代码审阅整改 — 🔄（2026-09-22 起；M9-A/M9-B ✅ 自动验收）

一轮静态代码审阅后的收尾质量整改，不引新功能。按优先级分批，每项独立自动验收。

**M9-A 资源协议路径穿越校验（P1，安全）**
- 现状：`assets.rs::asset_filename_from_uri` 百分号解码后不校验，恶意文档里 `http://donemd-asset.localhost/..%2f..%2fsecret` 理论上让 `stored_file_path` 拼出文档目录外路径。
- 方案：解码后拒绝含路径分隔符（`/` `\`）、`..` 段、或非纯文件名的输入（实际 asset 文件名恒为 `sha256hex.ext`，校验零误伤）；非法名直接当「找不到」返回 404。
- 验收：新增纯函数单测覆盖 `..`、`..%2f`、绝对路径、含分隔符、正常 hex 名；`cargo test` 全绿。

**M9-B 共享 reqwest 客户端（P1，性能）**
- 现状：`clients.rs::list_models` / `stream_completion` 各自 `reqwest::Client::new()`，每次重开连接池 + 重读 schannel 证书。
- 方案：`AppState` 持一个共享 `reqwest::Client`（内部 Arc，克隆廉价），`Client::for_provider` 或调用处复用。
- 验收：`cargo check` 零错误、`cargo test` 全绿；行为不变（仅复用连接）。

**M9-C SSE 单行缓冲上限（P2，健壮性）**
- 现状：`stream_completion` 的 `buf` 无换行符时无限增长，异常/恶意端点可发超长无换行行吃满内存。
- 方案：单行/累积缓冲加合理上限（如 1 MB），超限视为 `DecodeFailed` 中止。
- 验收：纯函数或集成测试覆盖超限中止；`cargo test` 全绿。

**M9-D（记录，暂不动）**：AI 60s 超时硬编码可提为 `AiConfig` 配置项；`Mutex::lock().unwrap()` 毒化面可评估 `parking_lot`。列为待排期，本轮不改。

**M9-E 飞书凭据存储越限修复（P0，用户报修）— ✅ 2026-09-23**
- 现象：真机登录失败，`登录失败:凭据存储失败:Attribute 'password encoded as UTF-16' is longer than platform limit of 2560 chars`。
- 根因：`KeyringCredentialStore` 把 `FeishuCredentials` 整块 JSON 存一条凭据管理器条目，而 keyring 3.6.3 的 windows-native 后端按 `password.encode_utf16().count() * 2 > CRED_MAX_CREDENTIAL_BLOB_SIZE(2560 字节)` 校验 —— 真实预算只有 **1280 个 UTF-16 码元**（报错文案的「2560 chars」是字节数，极易误读）。飞书 `user_access_token` + `refresh_token` 各数百字符，两枚加 JSON 外壳必然越界。
- 方案：存储层加分片适配器，凭据模型不动。索引条目写在规范账户名下（`{"v":2,"chunks":N}`），分片写在 `<account>.<i>`（Windows 下 keyring 的 `target_name = "{user}.{service}"`，故账户名后缀即条目区分位）。每片上限 1000 码元（留 22% 余量），片数上限 16。写序**先抹索引 → 写分片 → 最后写索引**：索引存在即「分片齐全」，中途崩溃退化成「未登录」而非指向半套分片的脏状态。`load` 保留旧式整存 blob 的兼容腿（靠索引与凭据 JSON 必填字段不相交来区分）。
- 验收（2026-09-23 全过）：`cargo test` 363 → **372 全绿**（新增 9 例：越限前提锁定、切片往返/边界/空载荷/不切断 char（含代理对 emoji）/预算小于单字符、索引与凭据 JSON 互不可解、分片账户名形状）。真机探针 2/2 通过（`cargo test feishu::credentials -- --ignored --test-threads=1`），含 900+900 字符超限载荷往返、以及「长载荷后存短载荷」验证多余分片被抹净不被新索引误纳。

## 评审记录

- 2026-09-16：完成现状调查，确认 Tauri 2 路线与发布范围。
- 2026-09-16：M0/M1/M2 自动验收通过；修复 11 处编译错误 + web 构建脚本跨平台化；测试 13 → 29（移植 macOS 关键用例），frontmatter 两处断言按 Swift 侧真实语义修正。
- 2026-09-16：M3 自动验收通过（测试 29 → 60）；修复设置窗误触脏关闭确认（按 label 限定 main）；设置窗不回填已存 key（安全面优于 macOS 的有意偏差）。
- 2026-09-16：踩坑记录——直接 `cargo build` 的 debug exe 指向 vite devUrl（localhost:5173），未起 dev server 时窗口「无法访问」；独立调试包须走 `npm run build-debug`（已写入 README 注意事项）。
- 2026-09-20：盘点发现 todo 落后于实做——大纲边栏（Windows）已在开发（Web 侧三件套完成、壳侧未接线），插入为 M3.5，不打乱 M4–M7 编号。顺带核实：`menu.rs`/`document.rs` 注释声称的 main.ts 菜单键 JS 兜底（`menuCommand` 信封）实际从未实现，Rust 侧分发臂空转；M3.5 的大纲开关把这条通道真正落地。
- 2026-09-20：M3.5 壳侧接线完成，自动验收全过。两个 Tauri 2 坑记入契约：`Emitter::emit` 在 webview 对象上调用是**全局广播**（不是自目标），多 webview 下必须 `emit_to(label)`；多 webview API（`add_child`/`get_webview`）需开 `tauri` 的 `unstable` feature。`bridge_dispatch` 现按发送者 label 路由 `editorReady`（侧栏 ready 回快照，主编辑器 ready 走文档加载）。
- 2026-09-17：M3 人工实测修复 4 项——① **keyring 3 默认特性不含平台后端**（内存 mock，key 随进程蒸发 → 401），须显式 `features = ["windows-native"]`，已加凭据探针测试；② 设置窗自绘下拉选中后面值不刷新（点击其实已生效但不可见，用户误点到 `mimo-v2.5-asr` → chat 接口 400），createSelect 改为自维护选中态；③ 模型回pin 大小写敏感（兜底常量 `MiMo-V2.5-Pro` vs 列表 `mimo-v2.5-pro`），改 `pin_model` 大小写不敏感匹配；④ 错误体提取兼容 `{"error":"str"}` / 顶层 `{"message":…}`，400 toast 现在带服务端原文。另：⌘ 提示经 `bridge.ts` 的 `MOD` 常量按平台输出（Win=`Ctrl+`）。测试 63 → 65。
- 2026-09-20：M3.6 自动验收通过。源码镜像栏 `source.rs` 启动即建常驻（macOS 无开关对齐）；`layout.rs` 升为唯一布局真源（三栏 `[outline?][visual][source]`，主 webview 常驻 `set_auto_resize(false)`，最小化 0×0 忽略）；同步走 `source-sync-<epoch>` 握手（epoch 防新建/打开途中旧回包落地，`source_sync_pending` 防重入且不丢更新——回包携带回复瞬间最新 doc）；`editorReady` 三臂路由 + `applyFold`/`scrollToHeading`/`badMathFormulas` fan-out 源码栏。冒烟确认双栏各半 + source 先于 main ready 时快照兜底正常。
- 2026-09-21：M3.6 人工实测修复——**Tauri CSP nonce 坑**：`tauri.conf.json` 写了 `style-src 'self' 'unsafe-inline'`，但 Tauri 的资产 CSP 改写会追加随机 `'nonce-…'`，nonce 在场即废掉 unsafe-inline（CSP 规范行为），CodeMirror style-mod 的运行时 `<style>` 被拦、CM6 布局样式全丢（源码栏只见折叠箭头不见字）。开 `dangerousDisableAssetCspModification: true` 让 CSP 按书写生效。排查手法备查：`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222` 起 CDP，`/json` 列 target，`Runtime.evaluate` 读 computed style，`Log.enable` + reload 抓 CSP 违规原文。
- 2026-09-21：M3.6 补齐 Mac 源码栏顶栏的 #82 token 读数——页内 28px 顶条（色点 + ≈tokens + 结论 + tooltip），`doc-size.ts` 移植 Swift 估算器并补 `charactersReadable`，vitest 35 → 48（新增 `doc-size.test.ts` 13 例对齐 Swift 测试），`.cm-content` padding-top 调 16px 使双栏首行同为 44px 齐平。CDP 实测（真实文档 10 KB / 6,659 字）：绿点 `rgb(51,183,89)`、「≈2.3K tokens」、「AI 可轻松读完整篇」、tooltip 三行齐全、首行 y=44、scroller flex 正常；cargo test 65/65。
- 2026-09-21：M4 自动验收 + CDP 实测通过。盘点发现 Range 代码已在但裸奔（无测试且整文件读）；移植 Swift 两侧测试共 33 例（65 → 98），修大视频 seek 全量入内存缺陷（`read_range` 有界读），`handle_asset_request` 拆薄壳提可测性，`extension_for_mime` 补 `image/jpg` 对齐。CDP：真实 H.264 经协议 readyState=4、`seeked` 精确落点、seek 请求 206 + Content-Range。🖐️ 剩两项人工：百 MB 大视频拖条流畅度、图片系统预览。
- 2026-09-21：M5 自动验收通过。`tauri build` 一次产出 MSI（4.36 MB）+ NSIS（3.49 MB）双安装包，release exe 7.59 MB 冒烟存活；版本号策略定为两侧共用营销版本号（1.0.0 对齐 Mac）；BUILD.md 补 Windows 全章节 + CI 骨架。🖐️ 剩实装实测（SmartScreen 提示属预期）。
- 2026-09-21：用户报修两项，均在设置窗，顺带挖出一个 M3 潜伏死锁。① 模型下拉长列表不可达——根因是 `.card` 的 `overflow:hidden` 裁剪 absolute 弹层（不是缺滚动条：`max-height/overflow-y` 早已在），修法 fixed 定位 + 视口感知布局（`select-pop-layout.ts` 纯函数 + 10 例测试，vitest 48 → 58）。② 修复过程中发现 toast「打开 Provider 设置」路径（`aiOpenSettings` 桥接信封）在 IPC 回调（主线程）里同步建窗 → WebView2 嵌套控制器创建死锁（窗口卡 about:blank、invoke 悬挂、主线程冻结）；插桩定位 `ThreadId(1)` 进 `build()` 不出；`std::thread::spawn` 建窗修复，CDP 复测 invoke 0.5s 内 resolved、设置窗正常加载。Tauri 2 坑记入契约面：**运行期建窗一律避开 IPC/WebView2 回调上下文**（启动期 `setup` 里建是安全的 direct 路径——outline/source 栏因此无恙）。cargo 98/98。
- 2026-09-22：M8 F1（飞书转换层）自动验收通过。四文件 1:1 移植（block/callout/encoder/converter，converter ~1,680 行含测试），占位块节点复用 `markdown::convert::feishu_placeholder_block`（改 `pub(crate)`）保证魔法注释与 Feishu API 两条路径落同一 Tiptap 节点形状。测试 136 → 228 全绿：converter 51 例（Swift 侧 19 结构 + 15 富块 + 2 全景夹具 + 12 占位 + 1 真机 wire 视频 + 2 Rust 补充）、placeholder 29 例（M2 只带过 1 fixture，本次补齐 parse/serialize/canonical_url/#89 url 回填/端到端全组）、frontmatter 11 例（feishu 子树解析 + merge 组，激活死码 merge）。`cargo check --tests` 死码警告 4 → 2（merge 消除；`cargo check` bin 目标的 ~87 条 feishu 过渡墙为预期，F2/F3 接线消除）。移植要点：表格脏输入钳位不 panic（Swift `cells[r*cols..<min]` 会越界）；blockquote 发射 ID 只分配一次（Swift 双分配为死码）；`#[derive(Copy)] ListKind` 解 E0369/E0507；测试里 `use crate::markdown::serialize` 会遮蔽本模块 `serialize`，别名引入解冲突。
- 2026-09-22：M8 F2.1+F2.2（网络+身份的网络面）自动验收通过。`api.rs`（trait + 错误全表 + 请求构造 + 退避，44 例）→ `http_client.rs`（泛型编排：401 静默刷新重试一次 / 退避至耗尽 / 下载旁路管线 / 分页聚合 / push 先删后建 / 上传 SHA-256 缓存，29 例）→ `oauth.rs`（authorize/交换/refresh 全依赖注入，12 例）→ `oauth_receiver.rs`（回环 TcpListener，纯函数判定组 11 例 + 真端口 2 例 `#[ignore]`），`credentials.rs`/`app_config.rs` 模型先行（keyring 与解析链在 F2.3）。272 → 324 全绿。三个 Windows/Rust 移植要点记档：① **AFIT trait 无 dyn 兼容面**——`Arc<dyn OAuthCallbackReceiver>` 持有 async trait 方法直接 E0038，手装箱 future（`Pin<Box<dyn Future + Send + 'a>>`）就地替换，不引 async-trait；② **立即 drop socket 触发 Windows RST**——回环应答写完就 drop，对端 `read_to_string` 收 10054 ConnectionReset（未 ACK 数据被 RST 丢弃），改分离任务优雅关闭（写 → FIN → 排干到 EOF）；③ **泛型 impl 块里无法以具体类型实例化类型参数**——`new()` 固定 `TokioSleeper` 须单独立 `impl<P> FeishuHttpApi<ReqwestTransport, P, TokioSleeper>` 块。另：`url_path` 初版只剥 query 没剥 scheme://authority，纯函数组首轮就抓出来了（真端口测试前先跑离线组的回报）。
- 2026-09-22：M8 F2.3 + F2 收尾（凭据/配置/管理器/设置页）自动验收通过。keyring 两条目（`feishu-oauth` 用户令牌 + `feishu-app-config` 应用凭证）、三级解析链（keyring > env > JSON，三字段缺一即整源失效、keyring 读失败不毒化链）、`manager.rs` AuthState 状态机 + 非机密镜像 `feishu-state.json`、`OAuthTokenProvider`（补 `force_refresh` 401 强刷腿）、`feishu/mod.rs` 5 条命令 + `state.rs`/`main.rs` 接线、设置窗升级通用 tab（AI/飞书同步，菜单与标题同步改「设置」）。324 → 360 全绿；bin 死码墙 ~175 → 147。安全红线落地：App Secret 只进 keyring 与请求体**永不回传 webview**（表单留空=沿用已存，对齐 ai Key 惯例）；镜像只存 4 非机密字段（测试断言无 secret/token 字样）。工具层坑记档：**harness 对工具 I/O 回显做敏感字段脱敏**（`.client_id`/`feishuDrafts.clientSecret`/`secretInput.value` 等显示为星号），本次既把真代码误判过垃圾、也把真垃圾误判过脱敏——判定铁律：`grep '\*\{3,\}'` 无输出 = 文件干净；有疑问用 `od -c` 看字节定案；修复只信编译器报错行号。Windows 分歧记录：① 配置文件 plist → JSON、无 bundle 兜底；② `settings_load` 实时读 keyring（CredMan 静默，Mac「启动只读镜像避 Keychain 弹窗」不适用，镜像只服务主窗徽标）；③ Mac 预填 secret vs Windows 不回填（安全收紧，有意偏差）。F2 人工清单（存凭证→登录→授权→回环→重启→refresh→登出→端口占用）待真凭据，可与 F4 端到端合并跑。
- 2026-09-22：Windows 平台面（M0–M7）收尾，README / BUILD.md 同步补齐 Windows 内容——README 新增「Windows 版」里程碑表（M0–M7 ✅，M8 飞书移植单列 🔄）、Windows 安装章节（MSI / NSIS + 未签名 SmartScreen 说明）、系统要求与隐私段落改为双端表述、技术栈补 Tauri 壳、已知差异改写（源码栏 / 快捷键 / 飞书未接入）；BUILD.md 补 Windows 工程结构树、Git Bash 下 `link.exe` 遮蔽 MSVC 链接器的 `cargo-win` 包装脚本、测试面数字（cargo 360 / vitest 58）、产物体积，并修正 macOS 章节里过时的「单页 ~290 KB」产物描述（现为四入口）。
- 2026-09-22：M9 代码审阅整改 M9-A + M9-B 自动验收通过（cargo 360 → 363，新增 3 例路径穿越守卫测试）。① **M9-A 资源协议路径穿越守卫**：`assets.rs::handle_asset_request` 在百分号解码后、`stored_file_path` 拼接前插入纯函数 `is_safe_asset_filename` 校验——拒绝含 `/`/`\`/NUL、纯点段（`.`/`..`）、前导 `~`、`C:` 盘符相对形式的解码名，非法即 404。恶意文档嵌 `http://donemd-asset.localhost/..%2f..%2fsecret` 解码成 `../../secret` 现被拦在资源目录内。实际 asset 名恒为 `<sha256>.<ext>`，守卫零误伤。② **M9-B 共享 reqwest 客户端**：`clients.rs::list_models`/`stream_completion` 原各自 `reqwest::Client::new()`（每次重开连接池 + 重读 schannel 证书），改为 `AppState.http` 单实例（reqwest 内部 Arc，克隆廉价），经 `Client::for_provider` 第四参注入，三处调用点（`start_request`/`ai_settings_save_key`/`ai_settings_refresh_models`）传 `state.http.clone()`。行为不变，仅复用连接。M9-C（SSE 单行缓冲上限）、M9-D（60s 超时提配置 / parking_lot 评估）未动，待排期。
- 2026-09-23：**M9-E 飞书登录失败修复（用户报修）**。报错 `凭据存储失败:Attribute 'password encoded as UTF-16' is longer than platform limit of 2560 chars` 的根因不在飞书逻辑，而在凭据存储层：keyring 3.6.3 windows-native 后端按 `encode_utf16().count() * 2 > CRED_MAX_CREDENTIAL_BLOB_SIZE(2560 **字节**)` 校验，真实预算只有 1280 UTF-16 码元，而两枚飞书 token 加 JSON 外壳必然越界——报错文案把字节数写成「2560 chars」，是这条 bug 最容易读错方向的地方（先读 keyring 源码定案判据，没按文案猜阈值）。修法是给存储层加分片适配器而非改凭据模型：索引条目 `{"v":2,"chunks":N}` 落规范账户名，分片落 `<account>.<i>`（Windows 下 `target_name = "{user}.{service}"`，账户名后缀即条目区分位，读源码确认而非假设）。写序**先抹索引 → 写分片 → 最后写索引**，使中途崩溃退化为「未登录」而非指向半套分片的脏状态；`load` 留旧式整存 blob 兼容腿（索引与凭据 JSON 必填字段不相交，天然可判别）。cargo 363 → 372 全绿，另跑真机探针 2/2（含 900+900 字符超限载荷往返、长载荷后存短载荷验证残留分片被抹净）——单元测试证明不了真机修好，这条路径必须打到真实 Credential Manager 上。
