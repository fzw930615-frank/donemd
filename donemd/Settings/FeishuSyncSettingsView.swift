import SwiftUI
import AppKit

/// Settings → 飞书同步 panel. Three sections per #52:
///   1. 登录态 — login button when logged out / tenant + logout when in
///   2. 同步根目录 — list with file-count badges, add/remove/reorder
///   3. 配额提示 — quota line; v2-10 step3 wires the real numbers
///
/// Hard non-goals (PRD § ADR-0001 says Done.md is a single-file editor):
///   ❌ no file list under each root
///   ❌ no "browse this folder in Done.md" entry point
///   ✅ explanatory copy makes this explicit
public struct FeishuSyncSettingsView: View {

    @ObservedObject var manager: FeishuSyncManager

    public init(manager: FeishuSyncManager) {
        self.manager = manager
    }

    public var body: some View {
        Form {
            appConfigSection
            authSection
            rootsSection
        }
        .formStyle(.grouped)
        .frame(minWidth: 540, minHeight: 540)
        .padding()
    }

    // MARK: - 0. app config (client_id / secret / redirect_uri)

    @State private var appConfigExpanded: Bool = false
    @State private var appConfigDraftClientID: String = ""
    @State private var appConfigDraftClientSecret: String = ""
    @State private var appConfigDraftRedirectURI: String = ""
    @State private var appConfigSaveError: String?
    @State private var appConfigSaveSuccess: Bool = false
    @State private var appConfigRedirectCopied: Bool = false

    private var appConfigSection: some View {
        Section {
            DisclosureGroup(
                isExpanded: $appConfigExpanded,
                content: {
                    VStack(alignment: .leading, spacing: 18) {
                        // 顶部两行灰字自成一组（组内紧凑），与下方各输入段之间留大间距
                        VStack(alignment: .leading, spacing: 8) {
                            Text("在[飞书开放平台](https://open.feishu.cn/app)的自建应用里拿到 App ID / App Secret / 重定向 URL。还没有飞书应用的话，先快速建一个「企业自建应用」。")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)

                            Label("仅存于本机 macOS 钥匙串，「马上做完」Done.md 不上传任何凭证到外部服务器。", systemImage: "lock.shield")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }

                        // ── App ID ──（左侧块标题走统一的 settingsItemTitle 规则：比外层
                        // section 标题更大更重；输入值本身保持常规字号）
                        VStack(alignment: .leading, spacing: 4) {
                            LabeledContent {
                                TextField("", text: $appConfigDraftClientID, prompt: Text("cli_xxxxxxxxxxxx"))
                                    .textFieldStyle(.roundedBorder)
                            } label: {
                                Text("App ID").settingsItemTitle()
                            }
                            Text("凭证与基础信息 → App ID")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }

                        // ── App Secret ──
                        VStack(alignment: .leading, spacing: 4) {
                            LabeledContent {
                                SecureField("", text: $appConfigDraftClientSecret, prompt: Text("粘贴 App Secret"))
                                    .textFieldStyle(.roundedBorder)
                            } label: {
                                Text("App Secret").settingsItemTitle()
                            }
                            Text("凭证与基础信息 → App Secret。注意密钥别外传")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }

                        // ── 重定向 URL（固定回调地址，只读）──
                        // 自建只读展示框：URL 文字靠左、复制图标靠右，中间用 Spacer 隔开，
                        // 彻底避免文字与图标重叠（之前 overlay 方案会压在一起）。
                        VStack(alignment: .leading, spacing: 4) {
                            LabeledContent {
                                HStack(spacing: 6) {
                                    Text(verbatim: appConfigDraftRedirectURI)
                                        .foregroundStyle(.secondary)
                                        .textSelection(.enabled)
                                        .lineLimit(1)
                                        .truncationMode(.middle)
                                    Spacer(minLength: 8)
                                    Button {
                                        let pasteboard = NSPasteboard.general
                                        pasteboard.clearContents()
                                        pasteboard.setString(appConfigDraftRedirectURI, forType: .string)
                                        appConfigRedirectCopied = true
                                    } label: {
                                        Image(systemName: appConfigRedirectCopied ? "checkmark" : "doc.on.doc")
                                            .foregroundStyle(appConfigRedirectCopied ? Color.green : Color.secondary)
                                    }
                                    .buttonStyle(.plain)
                                    .help("复制这串地址，去飞书后台「安全设置 → 重定向 URL」粘贴")
                                }
                                .padding(.horizontal, 8)
                                .padding(.vertical, 5)
                                .background(
                                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                                        .fill(Color(nsColor: .textBackgroundColor))
                                )
                                .overlay(
                                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                                        .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
                                )
                            } label: {
                                Text("重定向 URL").settingsItemTitle()
                            }
                            Text("复制 URL →「安全设置」→「重定向 URL」里登记一次")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }

                        HStack {
                            Button("保存到钥匙串") {
                                saveAppConfig()
                            }
                            .disabled(appConfigDraftClientID.isEmpty
                                      || appConfigDraftClientSecret.isEmpty
                                      || appConfigDraftRedirectURI.isEmpty)

                            if manager.keychainAppConfig() != nil {
                                Button("清除钥匙串里的凭证") {
                                    manager.clearAppConfig()
                                    appConfigDraftClientID = ""
                                    appConfigDraftClientSecret = ""
                                    appConfigDraftRedirectURI = ""
                                    appConfigSaveSuccess = false
                                }
                                .foregroundStyle(.red)
                            }

                            Spacer()
                            if appConfigSaveSuccess {
                                Label("已保存", systemImage: "checkmark.circle.fill")
                                    .foregroundStyle(.green)
                                    .font(.caption)
                            }
                        }

                        if let appConfigSaveError {
                            Text(appConfigSaveError)
                                .font(.caption)
                                .foregroundStyle(.red)
                        }
                    }
                    .padding(.top, 6)
                },
                label: {
                    // 整行标题栏都可点击展开/收起（不只是右侧箭头）
                    HStack {
                        Text("飞书应用凭证")
                        Spacer()
                        appConfigBadge
                    }
                    .contentShape(Rectangle())
                    .onTapGesture { withAnimation { appConfigExpanded.toggle() } }
                }
            )
            .onAppear { populateAppConfigDraftIfEmpty() }
        }
    }

    @ViewBuilder
    private var appConfigBadge: some View {
        if manager.keychainAppConfig() != nil {
            Label("Settings 已配置", systemImage: "key.fill")
                .labelStyle(.titleAndIcon)
                .font(.caption)
                .foregroundStyle(.green)
        } else if manager.currentAppConfig() != nil {
            Label("env / plist", systemImage: "key")
                .labelStyle(.titleAndIcon)
                .font(.caption)
                .foregroundStyle(.secondary)
        } else {
            Label("未配置", systemImage: "exclamationmark.triangle")
                .labelStyle(.titleAndIcon)
                .font(.caption)
                .foregroundStyle(.orange)
        }
    }

    private func populateAppConfigDraftIfEmpty() {
        // Don't clobber what the user is typing.
        guard appConfigDraftClientID.isEmpty,
              appConfigDraftClientSecret.isEmpty,
              appConfigDraftRedirectURI.isEmpty
        else { return }
        if let existing = manager.keychainAppConfig() {
            appConfigDraftClientID = existing.clientID
            appConfigDraftClientSecret = existing.clientSecret
            appConfigDraftRedirectURI = existing.redirectURI
        } else {
            // No saved config yet — prefill the fixed loopback callback so
            // the user doesn't have to know the exact string. Sourced from
            // the receiver's default rather than hardcoded, so it stays in
            // sync if the port ever changes.
            appConfigDraftRedirectURI = FeishuOAuthLoopbackReceiver().redirectURI
        }
    }

    private func saveAppConfig() {
        appConfigSaveError = nil
        appConfigSaveSuccess = false
        let result = manager.saveAppConfig(
            clientID: appConfigDraftClientID,
            clientSecret: appConfigDraftClientSecret,
            redirectURI: appConfigDraftRedirectURI
        )
        switch result {
        case nil:
            appConfigSaveSuccess = true
        case .missingField:
            appConfigSaveError = "三个字段都不能为空。"
        case .persistFailed(let detail):
            appConfigSaveError = "写入钥匙串失败：\(detail)"
        }
    }

    // MARK: - 1. auth

    @ViewBuilder
    private var authSection: some View {
        Section("飞书账号") {
            switch manager.authState {
            case .notConfigured:
                VStack(alignment: .leading, spacing: 6) {
                    Text("未配置飞书应用凭证")
                        .settingsItemTitle()
                    Text("先到上面的「飞书应用凭证」展开区填好 App ID / App Secret / Redirect URI 并保存到钥匙串，再回来登录。")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            case .loggedOut:
                VStack(alignment: .leading, spacing: 8) {
                    Text("尚未登录飞书")
                        .settingsItemTitle()
                    Button {
                        Task { await manager.login() }
                    } label: {
                        if manager.isLoggingIn {
                            HStack(spacing: 6) {
                                ProgressView().controlSize(.small)
                                Text("正在等待浏览器授权…")
                            }
                        } else {
                            Text("登录飞书")
                        }
                    }
                    .disabled(manager.isLoggingIn)
                    if let lastError = manager.lastError {
                        Text(lastError)
                            .font(.caption)
                            .foregroundStyle(.red)
                    }
                }
            case .loggedIn(let tenantKey):
                VStack(alignment: .leading, spacing: 8) {
                    HStack {
                        Image(systemName: "checkmark.circle.fill")
                            .foregroundStyle(.green)
                        Text("已登录").settingsItemTitle()
                    }
                    if let tenantKey, !tenantKey.isEmpty {
                        Text("租户：\(tenantKey)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else {
                        Text("租户：自建应用单租户").font(.caption).foregroundStyle(.secondary)
                    }
                    HStack {
                        Button("登出") {
                            Task { await manager.logout() }
                        }
                        Button("重新登录") {
                            Task {
                                await manager.logout()
                                await manager.login()
                            }
                        }
                    }
                }
            }
        }
    }

    // MARK: - 2. sync roots

    @State private var rootPendingDelete: URL?

    private var rootsSection: some View {
        Section {
            if manager.roots.isEmpty {
                Text("尚未添加同步根目录。粘贴飞书 URL 时只会查看你新建的当前文档；要让 Done.md 自动识别已有的本地副本，请添加此电脑里存放飞书绑定文档的目录。")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(manager.roots, id: \.path) { root in
                    rootRow(root)
                }
                .onMove { indices, newOffset in
                    var reordered = manager.roots
                    reordered.move(fromOffsets: indices, toOffset: newOffset)
                    Task { await manager.reorderRoots(reordered) }
                }
            }

            HStack {
                Button {
                    presentAddRootPanel()
                } label: {
                    Label("添加同步根目录", systemImage: "plus.circle")
                }
                Spacer()
            }

            if manager.roots.count > 10 {
                Text("⚠️ 已添加 \(manager.roots.count) 个同步根目录（建议 ≤ 10）。根目录越多，启动时扫描越慢。考虑合并相近目录或拆出单独账号。")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }

            Text("说明：Done.md 不会读取目录里非 .md 文件，也不展示文件列表——这只是后台索引，用来在你粘贴飞书 URL 时跳过新建、自动打开已有的本地副本。")
                .font(.caption)
                .foregroundStyle(.secondary)
        } header: {
            Text("同步根目录")
        }
        .alert(
            "确认删除同步根目录？",
            isPresented: rootDeleteBinding,
            presenting: rootPendingDelete
        ) { root in
            Button("删除", role: .destructive) {
                Task { await manager.removeRoot(root) }
                rootPendingDelete = nil
            }
            Button("取消", role: .cancel) {
                rootPendingDelete = nil
            }
        } message: { root in
            Text("\(root.path)\n\n目录里的文件不会被删除，只是 Done.md 不再扫描这里寻找飞书绑定。")
        }
    }

    @ViewBuilder
    private func rootRow(_ root: URL) -> some View {
        let count = manager.boundFileCounts[root] ?? 0
        let overLimit = count > SyncRootScanner.perRootFileLimit
        HStack(alignment: .firstTextBaseline) {
            Image(systemName: "folder")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(root.path)
                    .font(.body.monospaced())
                    .lineLimit(1)
                    .truncationMode(.middle)
                HStack(spacing: 6) {
                    Text("已索引 \(count) 个绑定飞书的文件")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    if overLimit {
                        Text("⚠️ 超出 \(SyncRootScanner.perRootFileLimit) 软上限")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    }
                }
            }
            Spacer()
            Button {
                rootPendingDelete = root
            } label: {
                Image(systemName: "minus.circle")
                    .foregroundStyle(.red)
            }
            .buttonStyle(.borderless)
            .help("删除此同步根目录")
        }
    }

    private var rootDeleteBinding: Binding<Bool> {
        Binding(
            get: { rootPendingDelete != nil },
            set: { newValue in
                if !newValue { rootPendingDelete = nil }
            }
        )
    }

    private func presentAddRootPanel() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "添加为同步根目录"
        panel.message = "选择存放飞书绑定 .md 文件的目录"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        Task {
            if let error = await manager.addRoot(url) {
                let alert = NSAlert()
                alert.messageText = "添加失败"
                switch error {
                case .alreadyAdded:
                    alert.informativeText = "这个目录已经在同步根目录列表里了。"
                case .notADirectory:
                    alert.informativeText = "选择的不是目录：\(url.path)"
                case .persistFailed(let detail):
                    alert.informativeText = "保存配置失败：\(detail)"
                }
                alert.runModal()
            }
        }
    }

    // ADR-0006 § Settings UI originally listed a quota section pulling
    // "本月已调用 N / M" via Feishu API. Investigation in v2-10 step3
    // (2026-05-29) found Feishu OpenAPI doesn't expose any such query
    // endpoint — limit headers come back only on rate-limited 429
    // responses, and tenant-level monthly quota is visible only in the
    // Feishu admin console. The section was removed rather than left as
    // a permanent "暂不可用" placeholder; users who hit limits get the
    // existing 限流 dialog from PushCommand / PullCommand and see the
    // actual numbers in the Feishu console.
}

// MARK: - 统一的「灰色块内 item 标题」样式（固定规则）

extension View {
    /// 固定排版规则：分组表单（`.formStyle(.grouped)`）里灰色卡片**内部**的
    /// item 标题 / 关键状态文字，一律比外层 section 标题**更大、更重、更明显**。
    /// 全部 Settings 面板（飞书同步 + AI Provider）统一走这一个修饰符，
    /// 改字号只改这里一处。
    func settingsItemTitle() -> some View {
        self.font(.title3.bold())
    }
}
