# Bridge 契约 — Web ↔ 原生(Windows/Tauri 移植依据)

信封格式(双向一致):`{ "version": 1, "type": string, "payload": any }`

- **JS → 原生**:`window.webkit.messageHandlers.donemd.postMessage(JSON)`
  → Tauri 下由 shim 转为 `invoke('bridge_dispatch', { json })`。
- **原生 → JS**:macOS 用 `evaluateJavaScript("window.donemdBridge.receive({...})")`
  → Tauri 下由 Rust `emit_to(label, "bridge", envelope)`(定向;`Emitter::emit`
  实为全局广播,多 webview 下不用)+ shim 监听调 `receive()`。
- 请求/响应:payload 带 `requestId`,原生回指定 `replyType` 且携带同 `requestId`。

**Tauri 2 已知坑(踩过,勿再踩)**

1. **运行期建窗不可在 IPC 回调里同步做**:`bridge_dispatch` 命令跑在主线程的
   WebView2 `WebResourceReceived` 回调里,其中 `WebviewWindowBuilder::build()`
   的控制器创建会嵌套等待自身完成 → 死锁(窗口卡 about:blank、invoke 悬挂、
   主窗冻结)。启动期 `setup` 里建窗是安全的(direct 路径);运行期一律
   `std::thread::spawn` 建窗走事件循环代理(见 `ai::open_settings`,2026-09-21)。
2. `Emitter::emit` 在 webview 对象上调用实为**全局广播**,多 webview 必须 `emit_to(label)`。
3. 多 webview API(`add_child`/`get_webview`)需开 tauri 的 `unstable` feature。

## JS → 原生(17 种)

| type | payload | 原生职责 | Windows 映射 |
|---|---|---|---|
| `editorReady` | null | 推送 `loadDocument`;重发折叠状态 | Rust: **按发送者 label 路由**——主编辑器 ready → loadDocument;大纲侧栏 ready → 回 `outlineSet`+`outlineActive` 快照 |
| `documentChanged` | null | 标脏;同步到源码栏 | Rust: 标脏(Windows 首版单栏,仅标脏) |
| `importImage` | `{requestId, mime, base64}` | 存 assets/,回 `imageImported` | Rust: sha256 去重写盘 |
| `previewImage` | `{src}` | 系统预览图片 | Rust: 调系统默认查看器 |
| `openLink` | `{href}` | 分类打开 web/本地链接;feishu:// 打开绑定文档 | Rust: opener crate |
| `outlineChanged` | `{headings:[{level,text,index}]}` | 更新大纲 | Rust: 存 state + 中继 `outlineSet` 给大纲侧栏(M3.5) |
| `activeHeadingChanged` | `{index:int\|null}` | 大纲滚动高亮 | Rust: 存 state + 中继 `outlineActive`(M3.5) |
| `outlineJump` | `{index}` | (macOS 由 SwiftUI 侧栏直接消费,不过桥) | Rust: 大纲侧栏行点击 → `scrollToHeading` 发主编辑器(M3.5;源码镜像栏未做,只发 Visual) |
| `badMathFormulas` | `{latex:[string]}` | 源码栏标红 | 首版忽略(无双栏) |
| `foldToggled` | `{ordinal, collapse}` | 更新折叠集合并回播 `applyFold` | Rust: 状态+回播 |
| `foldReplace` | `{collapsed:[int]}` | 整体替换折叠集合 | 同上 |
| `menuCommand` | `{command}` | (macOS 无;菜单键的 JS 兜底通道) | Rust: new/open/save/saveAs/toggleOutline 分发(M3.5 起真正使用) |
| `aiCommand` | `{streamId, command, arg?, selection?, paragraph?, before?, after?, provider?}` | 组 prompt→SSE 流→回 aiStream* | Rust: reqwest SSE |
| `aiCancel` | null | 取消流 | Rust: abort |
| `aiRetry` | `{streamId}` | 重发上次请求 | Rust: 缓存重发 |
| `aiOpenSettings` | null | 打开设置窗 | Rust: 设置窗口/对话框 |
| `aiProvidersQuery` | `{requestId}` | 回 `aiProvidersReply` | Rust: 查 keyring |
| `feishuImportFromUrl` | `{url}` | URL 弹窗提交 → 解析 token → 拉取 → 弹保存位置 | M8 F4-b。token 抠取在 `feishu::url::extract_doc_token`(接受完整 URL 或裸 token);前端只校验非空,不重复实现判定 |

## 原生 → JS(16 种)

| type | payload | 触发 | 说明 |
|---|---|---|---|
| `loadDocument` | Tiptap doc JSON | editorReady / 外部变更重载 | |
| `imageImported` | `{requestId, success, assetURL?, markdownPath?, error?}` | importImage 应答 | assetURL=`donemd-asset://<sha256>.<ext>` |
| `aiStreamStart` | `{streamId}` | 流开始 | |
| `aiStreamToken` | `{streamId, text}` | 每个 token | |
| `aiStreamComplete` | `{streamId, node}` | 完成,node=Tiptap JSON | |
| `aiStreamError` | `{streamId, message, canOpenSettings?}` | 失败 | |
| `aiStreamBusy` | `{}` | 并发拒绝 | |
| `aiStreamDegrade` | `{streamId}` | 8K 降级提示 | |
| `aiStreamNotApplicable` | `{streamId, message}` | 命令不适用(如转表格) | |
| `aiProvidersReply` | `{requestId, providers:[{id,name}], default}` | 查询应答 | |
| `formatCommand` | `{command, value?}` | 菜单加粗/斜体等 | 首版可用快捷键在 JS 内处理 |
| `insertImage/insertVideo/insertTable` | 各异 | 菜单插入 | |
| `scrollToHeading` | `{index}` | 大纲点击 | |
| `applyFold` | `{collapsed:[int]}` | 折叠状态广播 | |
| `outlineSet` | `{headings:[{level,text,index}]}` | 大纲全量替换(中继 outlineChanged / 侧栏 editorReady 快照) | M3.5,只发大纲侧栏 |
| `outlineActive` | `{index:int\|null}` | scrollspy 高亮(中继 activeHeadingChanged) | M3.5,只发大纲侧栏 |
| `feishuSyncToast` | `{message, kind}` | 飞书同步进度/结果 | M8 F4。`kind` ∈ `progress`\|`done`\|`error`,与 web 侧 `renderToast` 的联合类型逐字对齐;复用 AI toast 组件。终态 5s 自动消失,progress 常驻待顶替 |
| `feishuOpenImportModal` | `{}` | 菜单「从飞书链接新建…」 | M8 F4-b。原生菜单只发信号,URL 输入框在 web 层(`feishu-import-modal.ts`);提交回 `feishuImportFromUrl` |

## 资源协议

- `donemd-asset://<sha256>.<ext>` → 文档目录 `assets/` 下文件;未保存文档用临时目录。
  Tauri: `register_uri_scheme_protocol("donemd-asset", …)`,需支持 HTTP Range(视频 seek)。
- Markdown 落盘路径 `./assets/<file>` 不变,保证与 Mac 版文档互通。

## Markdown ↔ Tiptap 转换(Swift → Rust 移植清单)

- `ASTConverter.swift`(664 行,swift-markdown)→ Rust `pulldown-cmark` 事件流重建
- `Serializer.swift`(334 行,canonical 输出)→ Rust 直译
- `FrontmatterEngine.swift`(482 行,YAML + 飞书绑定)→ serde_yaml
- `TiptapNode.swift`(139 行)→ serde_json Value 或结构体
- 特例:块级数学 `$$…$$` 需按源码行还原(swift-markdown 会吃掉反斜杠,
  pulldown-cmark 同样,照原方案从源文本按行截取);`<video>` 单行嗅探;独立图片行。

## 结论

契约面小而稳定,JS 侧零改动即可桥到 Tauri。工作量集中在 Rust 侧的
MarkdownEngine 移植(约 2100 行 Swift)与飞书 Block 转换层。
