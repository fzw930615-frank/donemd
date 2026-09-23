// 设置 window — Windows counterpart of `AIProviderSettingsView.swift` +
// `FeishuSyncSettingsView.swift`. Two top-level tabs (AI / 飞书同步) share
// one window, mirroring the single macOS settings entry.
//
// Unlike the editor this window talks to native through dedicated Tauri
// commands (`ai_settings_*` / `feishu_*`) instead of bridge envelopes: no
// document state lives here, and invoke() already gives request/response.
// Secrets (API Key / App Secret) travel JS → Credential Manager only; the
// native side never returns them, so a configured provider's field stays
// empty (re-saving with an empty field reuses the stored value —
// endpoint-only edits don't force re-pasting).

import './settings.css';
import { computeSelectPopLayout } from './select-pop-layout';

declare global {
  interface Window {
    __TAURI__?: {
      core: { invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown> };
    };
  }
}

interface ProviderView {
  id: string;
  name: string;
  recommended: boolean;
  configured: boolean;
  model_id: string;
  endpoint: string;
  model_list: string[];
}

interface SettingsPayload {
  defaultProvider: string;
  contextRange: number;
  providers: ProviderView[];
}

interface SaveResult {
  ok: boolean;
  error?: string;
  models?: string[];
  degraded?: boolean;
  fallback?: string;
  /** Attempted model-list URL — shown under fetch errors for debuggability. */
  url?: string;
}

/** `feishu_settings_load` payload — 非机密字段 only;secret 永不回传。 */
interface FeishuSettings {
  authState: 'loggedIn' | 'loggedOut' | 'notConfigured';
  tenantKey: string | null;
  isLoggingIn: boolean;
  lastError: string | null;
  /** keyring = Settings 保存的;envOrFile = 环境变量/手写文件;none。 */
  source: 'keyring' | 'envOrFile' | 'none';
  clientId: string | null;
  redirectUri: string | null;
  defaultRedirectUri: string;
}

/** Per-card transient UI state (drafts, status line, busy flag). */
interface CardState {
  keyDraft: string;
  endpointDraft: string;
  expanded: boolean;
  endpointExpanded: boolean;
  busy: boolean;
  status: string;
  statusIsError: boolean;
  statusUrl: string;
}

let settings: SettingsPayload | null = null;
const cardStates = new Map<string, CardState>();

function cardState(id: string): CardState {
  let s = cardStates.get(id);
  if (!s) {
    s = {
      keyDraft: '',
      endpointDraft: '',
      expanded: false,
      endpointExpanded: false,
      busy: false,
      status: '',
      statusIsError: false,
      statusUrl: '',
    };
    cardStates.set(id, s);
  }
  return s;
}

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const tauri = window.__TAURI__;
  if (!tauri) throw new Error('not running inside Tauri');
  return (await tauri.core.invoke(cmd, args)) as T;
}

async function update(field: string, provider: string | null, value: unknown): Promise<void> {
  await invoke('ai_settings_update', { field, provider, value });
}

// MARK: - render

/** 顶层分段 tab(单窗口单入口,对齐 macOS 设置)。 */
let activeTab: 'ai' | 'feishu' = 'ai';

function render(): void {
  const app = document.getElementById('app');
  if (!app || !settings) return;
  app.textContent = '';
  app.append(tabBar(), activeTab === 'ai' ? aiPane() : feishuPane());
}

function aiPane(): HTMLElement {
  const pane = document.createElement('div');
  pane.append(
    defaultProviderSection(),
    contextRangeSection(),
    providerCardsSection(),
  );
  return pane;
}

function tabBar(): HTMLElement {
  const bar = document.createElement('nav');
  bar.className = 'tabs';
  const tabs: Array<{ id: 'ai' | 'feishu'; label: string }> = [
    { id: 'ai', label: 'AI' },
    { id: 'feishu', label: '飞书同步' },
  ];
  for (const t of tabs) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'tab' + (activeTab === t.id ? ' active' : '');
    b.textContent = t.label;
    b.addEventListener('click', () => {
      if (activeTab === t.id) return;
      activeTab = t.id;
      // 飞书状态懒加载:首次切入才碰凭据管理器。
      if (t.id === 'feishu' && !feishuLoaded) void refreshFeishu();
      render();
    });
    bar.append(b);
  }
  return bar;
}

function section(title: string, ...children: HTMLElement[]): HTMLElement {
  const sec = document.createElement('section');
  const h = document.createElement('h2');
  h.textContent = title;
  sec.append(h, ...children);
  return sec;
}

function caption(text: string): HTMLElement {
  const p = document.createElement('p');
  p.className = 'caption';
  p.textContent = text;
  return p;
}

// MARK: - custom select

interface SelectOption {
  value: string;
  label: string;
}

/**
 * Themeable replacement for the native `<select>`: WebView2 renders native
 * dropdown popups with the browser-process theme, which stays light on a
 * dark page regardless of `color-scheme`. This component keeps face + popup
 * inside the page so both follow the CSS.
 */
function createSelect(
  options: SelectOption[],
  current: string,
  disabled: boolean,
  onChange: (value: string) => void,
): HTMLElement {
  const root = document.createElement('div');
  root.className = 'xselect';

  const face = document.createElement('button');
  face.type = 'button';
  face.className = 'xselect-face';
  face.disabled = disabled;
  const faceLabel = document.createElement('span');
  faceLabel.className = 'xselect-face-label';
  faceLabel.textContent = options.find((o) => o.value === current)?.label ?? current;
  const chev = document.createElement('span');
  chev.className = 'chevron';
  chev.textContent = '▾';
  face.append(faceLabel, chev);

  const pop = document.createElement('div');
  pop.className = 'xselect-pop';
  pop.hidden = true;

  let currentValue = current;

  const close = () => {
    pop.hidden = true;
    document.removeEventListener('mousedown', onDocDown);
    window.removeEventListener('resize', close);
    window.removeEventListener('scroll', onScrollOutside, true);
  };
  const onDocDown = (e: Event) => {
    if (!root.contains(e.target as Node)) close();
  };
  // Fixed-position popups don't track their face — any scroll (page or an
  // ancestor card) moves the anchor out from under it. Scrolling *inside*
  // the popup itself is the user reaching option 8+, so that stays open.
  const onScrollOutside = (e: Event) => {
    if (e.target instanceof Node && pop.contains(e.target)) return;
    close();
  };

  const items: HTMLButtonElement[] = [];
  for (const o of options) {
    const item = document.createElement('button');
    item.type = 'button';
    item.className = 'xselect-opt' + (o.value === current ? ' selected' : '');
    item.textContent = o.label;
    item.addEventListener('click', () => {
      close();
      if (o.value === currentValue) return;
      currentValue = o.value;
      // The component owns its face — callers persist via onChange but don't
      // re-render (render() would blow away in-flight drafts on the card).
      faceLabel.textContent = o.label;
      for (const el of items) el.classList.toggle('selected', el === item);
      onChange(o.value);
    });
    items.push(item);
    pop.append(item);
  }

  face.addEventListener('click', () => {
    if (pop.hidden) {
      // Provider cards clip overflow (rounded corners); a fixed-position
      // popup escapes that and gets clamped to the viewport instead — see
      // select-pop-layout.ts.
      const layout = computeSelectPopLayout(face.getBoundingClientRect(), window.innerHeight);
      pop.style.left = `${layout.left}px`;
      pop.style.width = `${layout.width}px`;
      pop.style.top = layout.top === null ? '' : `${layout.top}px`;
      pop.style.bottom = layout.bottom === null ? '' : `${layout.bottom}px`;
      pop.style.maxHeight = `${layout.maxHeight}px`;
      pop.hidden = false;
      // Land the current model mid-list — an 8+ model list shouldn't make
      // the user scroll from item #1 to reach a selection further down.
      const selected = items.find((el) => el.classList.contains('selected'));
      if (selected) {
        pop.scrollTop = selected.offsetTop - (pop.clientHeight - selected.clientHeight) / 2;
      }
      document.addEventListener('mousedown', onDocDown);
      window.addEventListener('resize', close);
      window.addEventListener('scroll', onScrollOutside, true);
    } else {
      close();
    }
  });
  root.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') close();
  });

  root.append(face, pop);
  return root;
}

function defaultProviderSection(): HTMLElement {
  const row = document.createElement('div');
  row.className = 'row';
  const label = document.createElement('label');
  label.textContent = '默认 Provider';
  const options = settings!.providers.map((p) => ({ value: p.id, label: p.name }));
  const select = createSelect(options, settings!.defaultProvider, false, (value) => {
    settings!.defaultProvider = value;
    void update('defaultProvider', null, value);
  });
  row.append(label, select);
  return section(
    '默认 Provider',
    row,
    caption('所有 AI 助手调用都走这个 Provider。一次设一个，不按命令切换。'),
  );
}

function contextRangeSection(): HTMLElement {
  const row = document.createElement('div');
  row.className = 'row';
  const label = document.createElement('label');
  label.textContent = '前后段落数';
  const options: SelectOption[] = [
    { value: '0', label: '0（仅选区所在段）' },
    { value: '1', label: '1（默认）' },
    { value: '2', label: '2' },
    { value: '3', label: '3' },
  ];
  const select = createSelect(options, String(settings!.contextRange), false, (value) => {
    settings!.contextRange = Number(value);
    void update('contextRange', null, Number(value));
  });
  row.append(label, select);
  return section(
    'AI 上下文',
    row,
    caption(
      '改写 / 转换类命令随选区一起发送的上下文范围。翻译只发选区、续写发整篇，不受此项影响。文档过长时自动降级为仅选区。',
    ),
  );
}

function providerCardsSection(): HTMLElement {
  const cards = settings!.providers.map((p) => providerCard(p));
  return section('Provider 配置', ...cards);
}

function providerCard(p: ProviderView): HTMLElement {
  const st = cardState(p.id);
  const card = document.createElement('div');
  card.className = 'card';

  // Header row: name, 推荐 pill, configured check, expand toggle.
  const header = document.createElement('button');
  header.className = 'card-header';
  header.type = 'button';
  const name = document.createElement('span');
  name.className = 'card-name';
  name.textContent = p.name;
  header.append(name);
  if (p.recommended) {
    const pill = document.createElement('span');
    pill.className = 'pill';
    pill.textContent = '推荐';
    header.append(pill);
  }
  const spacer = document.createElement('span');
  spacer.className = 'spacer';
  header.append(spacer);
  if (p.configured) {
    const check = document.createElement('span');
    check.className = 'configured';
    check.textContent = '✓ 已配置';
    header.append(check);
  }
  const chevron = document.createElement('span');
  chevron.className = 'chevron';
  chevron.textContent = st.expanded ? '▾' : '▸';
  header.append(chevron);
  header.addEventListener('click', () => {
    st.expanded = !st.expanded;
    render();
  });
  card.append(header);

  if (st.expanded) {
    card.append(providerCardBody(p, st));
  }
  return card;
}

function providerCardBody(p: ProviderView, st: CardState): HTMLElement {
  const body = document.createElement('div');
  body.className = 'card-body';

  // API Key
  const keyLabel = fieldLabel('API Key');
  const keyInput = document.createElement('input');
  keyInput.type = 'password';
  keyInput.placeholder = p.configured ? '已保存（输入新 Key 以替换）' : '粘贴 API Key';
  keyInput.value = st.keyDraft;
  keyLabel.append(keyInput);
  body.append(keyLabel);

  // Model dropdown — greyed single fallback until a live list was fetched.
  const modelLabel = fieldLabel('Model');
  const models = p.model_list.length > 0 ? p.model_list : [p.model_id];
  const modelSelect = createSelect(
    models.map((m) => ({ value: m, label: m })),
    p.model_id,
    p.model_list.length === 0,
    (value) => {
      p.model_id = value;
      void update('modelId', p.id, value);
    },
  );
  modelLabel.append(modelSelect);
  body.append(modelLabel);

  // 高级：Endpoint
  const details = document.createElement('details');
  details.open = st.endpointExpanded;
  details.addEventListener('toggle', () => {
    st.endpointExpanded = details.open;
  });
  const summary = document.createElement('summary');
  summary.textContent = '高级：Endpoint';
  const endpointInput = document.createElement('input');
  endpointInput.type = 'text';
  endpointInput.placeholder = p.endpoint; // native resolves empty → 官方默认
  endpointInput.value = st.endpointDraft;
  endpointInput.addEventListener('input', () => {
    st.endpointDraft = endpointInput.value;
  });
  details.append(summary, endpointInput, caption('留空使用官方默认。可改以对接 Azure / 代理 / 自建网关。'));
  body.append(details);

  // Actions
  const actions = document.createElement('div');
  actions.className = 'actions';
  const saveBtn = document.createElement('button');
  saveBtn.type = 'button';
  saveBtn.className = 'primary';
  saveBtn.textContent = st.busy ? '测试连接中…' : '测试连接 + 保存';
  const syncSaveDisabled = () => {
    saveBtn.disabled = st.busy || (!p.configured && st.keyDraft.trim() === '');
  };
  syncSaveDisabled();
  // render() only runs on state transitions — keep the button live while the
  // user types (a fresh unconfigured provider starts with an empty draft).
  keyInput.addEventListener('input', () => {
    st.keyDraft = keyInput.value;
    syncSaveDisabled();
  });
  saveBtn.addEventListener('click', () => void testAndSave(p, st));
  actions.append(saveBtn);

  if (p.model_list.length > 0) {
    const refetchBtn = document.createElement('button');
    refetchBtn.type = 'button';
    refetchBtn.className = 'link';
    refetchBtn.textContent = '重新拉模型列表';
    refetchBtn.disabled = st.busy;
    refetchBtn.addEventListener('click', () => void refetch(p, st));
    actions.append(refetchBtn);
  }

  const spacer = document.createElement('span');
  spacer.className = 'spacer';
  actions.append(spacer);

  if (p.configured) {
    const deleteBtn = document.createElement('button');
    deleteBtn.type = 'button';
    deleteBtn.className = 'link danger';
    deleteBtn.textContent = '删除配置';
    deleteBtn.disabled = st.busy;
    deleteBtn.addEventListener('click', () => void clearKey(p, st));
    actions.append(deleteBtn);
  }
  body.append(actions);

  if (st.status) {
    const status = document.createElement('p');
    status.className = st.statusIsError ? 'status error' : 'status';
    status.textContent = st.status;
    body.append(status);
    if (st.statusUrl) {
      const url = document.createElement('p');
      url.className = 'status status-url';
      url.textContent = `请求地址：${st.statusUrl}`;
      body.append(url);
    }
  }
  return body;
}

function fieldLabel(text: string): HTMLLabelElement {
  const label = document.createElement('label');
  label.className = 'field';
  const span = document.createElement('span');
  span.className = 'field-name';
  span.textContent = text;
  label.append(span);
  return label;
}

// MARK: - actions

async function testAndSave(p: ProviderView, st: CardState): Promise<void> {
  st.busy = true;
  st.status = '';
  st.statusUrl = '';
  render();
  let refresh = false;
  try {
    // Endpoint override applies at save time, same as the Swift testAndSave.
    await update('endpoint', p.id, st.endpointDraft.trim());
    const result = await invoke<SaveResult>('ai_settings_save_key', {
      provider: p.id,
      key: st.keyDraft,
    });
    if (!result.ok) {
      st.status = result.error ?? '保存失败';
      st.statusIsError = true;
    } else if (result.degraded) {
      st.status = `已保存，但无法拉模型列表（${result.error}），使用默认 model ${result.fallback}`;
      st.statusUrl = result.url ?? '';
      st.statusIsError = true;
      st.keyDraft = '';
      refresh = true;
    } else {
      st.status = `已保存 · 拉到 ${result.models?.length ?? 0} 个 model`;
      st.statusIsError = false;
      st.keyDraft = '';
      refresh = true;
    }
  } catch (e) {
    st.status = String(e);
    st.statusIsError = true;
  }
  st.busy = false;
  if (refresh) {
    await reload();
  } else {
    render();
  }
}

async function refetch(p: ProviderView, st: CardState): Promise<void> {
  st.busy = true;
  st.status = '';
  st.statusUrl = '';
  render();
  let refresh = false;
  try {
    const result = await invoke<SaveResult>('ai_settings_refresh_models', { provider: p.id });
    if (result.ok) {
      st.status = `已刷新 · ${result.models?.length ?? 0} 个 model`;
      st.statusIsError = false;
      refresh = true;
    } else {
      st.status = `无法拉模型列表（${result.error}）`;
      st.statusUrl = result.url ?? '';
      st.statusIsError = true;
    }
  } catch (e) {
    st.status = String(e);
    st.statusIsError = true;
  }
  st.busy = false;
  if (refresh) {
    await reload();
  } else {
    render();
  }
}

async function clearKey(p: ProviderView, st: CardState): Promise<void> {
  st.busy = true;
  render();
  await invoke('ai_settings_clear_key', { provider: p.id });
  st.keyDraft = '';
  st.status = '已删除配置';
  st.statusIsError = false;
  st.busy = false;
  await reload();
}

async function reload(): Promise<void> {
  settings = await invoke<SettingsPayload>('ai_settings_load');
  // reload() re-renders after the caller's status line was set — the status
  // survives because cardStates persist across renders.
  render();
}

// MARK: - 飞书同步(FeishuSyncSettingsView.swift 对应物,同步根目录段后置)

let feishu: FeishuSettings | null = null;
let feishuLoaded = false;
let feishuBusy = false;
/** 登录 invoke 在途(最长 5 分钟)— 按钮转「正在等待浏览器授权…」。 */
let feishuLoggingIn = false;
let feishuStatus = '';
let feishuStatusIsError = false;
/** null = 未播种;首次加载按 source 决定(未配置 → 展开引导用户)。 */
let feishuCardExpanded: boolean | null = null;
/** 表单草稿跨 render 存活(对齐 cardStates;secret 恒不回填,留空 = 沿用)。 */
const feishuDrafts = { clientId: '', clientSecret: '', redirectUri: '', seeded: false };

async function refreshFeishu(): Promise<void> {
  feishu = await invoke<FeishuSettings>('feishu_settings_load');
  feishuLoaded = true;
  if (!feishuDrafts.seeded) {
    feishuDrafts.clientId = feishu.clientId ?? '';
    feishuDrafts.redirectUri = feishu.redirectUri ?? feishu.defaultRedirectUri;
    feishuDrafts.seeded = true;
    if (feishuCardExpanded === null) feishuCardExpanded = feishu.source === 'none';
  }
  // reload() re-renders after the caller's status line was set — the status
  // survives because feishuStatus/feishuDrafts are module state.
  render();
}

function feishuPane(): HTMLElement {
  const pane = document.createElement('div');
  if (!feishu) {
    pane.append(caption('加载中…'));
    return pane;
  }
  pane.append(feishuConfigCard(feishu), feishuAuthSection(feishu));
  return pane;
}

function feishuConfigCard(f: FeishuSettings): HTMLElement {
  const card = document.createElement('div');
  card.className = 'card';

  const header = document.createElement('button');
  header.type = 'button';
  header.className = 'card-header';
  const name = document.createElement('span');
  name.className = 'card-name';
  name.textContent = '飞书应用凭证';
  header.append(name);
  if (f.source === 'keyring') {
    const ok = document.createElement('span');
    ok.className = 'configured';
    ok.textContent = '✓ 已配置';
    header.append(ok);
  } else {
    const pill = document.createElement('span');
    pill.className = 'pill muted';
    pill.textContent = f.source === 'envOrFile' ? 'env / 文件' : '未配置';
    header.append(pill);
  }
  const spacer = document.createElement('span');
  spacer.className = 'spacer';
  const chevron = document.createElement('span');
  chevron.className = 'chevron';
  chevron.textContent = feishuCardExpanded ? '▾' : '▸';
  header.append(spacer, chevron);
  header.addEventListener('click', () => {
    feishuCardExpanded = !feishuCardExpanded;
    render();
  });
  card.append(header);

  if (feishuCardExpanded) card.append(feishuConfigCardBody(f));
  return card;
}

function feishuConfigCardBody(f: FeishuSettings): HTMLElement {
  const body = document.createElement('div');
  body.className = 'card-body';

  // 保存按钮先行 — 两个输入框的 input 事件驱动其禁用态。
  const saveBtn = document.createElement('button');
  saveBtn.type = 'button';
  saveBtn.className = 'primary';
  saveBtn.textContent = feishuBusy ? '保存中…' : '保存到凭据管理器';
  const syncSaveDisabled = () => {
    // keyring 已有条目时 secret 留空 = 沿用已存;否则三字段必须齐全。
    saveBtn.disabled =
      feishuBusy ||
      feishuDrafts.clientId.trim() === '' ||
      feishuDrafts.redirectUri.trim() === '' ||
      (f.source !== 'keyring' && feishuDrafts.clientSecret.trim() === '');
  };
  syncSaveDisabled();
  saveBtn.addEventListener('click', () => void feishuSaveConfig());

  // App ID
  const idLabel = fieldLabel('App ID');
  const idInput = document.createElement('input');
  idInput.type = 'text';
  idInput.placeholder = 'cli_…';
  idInput.value = feishuDrafts.clientId;
  idInput.addEventListener('input', () => {
    feishuDrafts.clientId = idInput.value;
    syncSaveDisabled();
  });
  idLabel.append(idInput);
  body.append(idLabel);

  // App Secret — 永不回填;留空 = 沿用已存(对齐 AI Key 惯例)。
  const secretLabel = fieldLabel('App Secret');
  const secretInput = document.createElement('input');
  secretInput.type = 'password';
  secretInput.placeholder =
    f.source === 'keyring' ? '已保存（输入新 Secret 以替换）' : '粘贴 App Secret';
  secretInput.value = feishuDrafts.clientSecret;
  secretInput.addEventListener('input', () => {
    feishuDrafts.clientSecret = secretInput.value;
    syncSaveDisabled();
  });
  secretLabel.append(secretInput);
  body.append(secretLabel);

  // 重定向 URL — 须与开放平台「安全设置 → 重定向 URL」逐字一致。
  const redirectLabel = fieldLabel('重定向 URL');
  const redirectRow = document.createElement('div');
  redirectRow.className = 'field-row';
  const redirectInput = document.createElement('input');
  redirectInput.type = 'text';
  redirectInput.value = feishuDrafts.redirectUri;
  redirectInput.addEventListener('input', () => {
    feishuDrafts.redirectUri = redirectInput.value;
    syncSaveDisabled();
  });
  const copyBtn = document.createElement('button');
  copyBtn.type = 'button';
  copyBtn.className = 'link';
  copyBtn.textContent = '复制';
  copyBtn.addEventListener('click', () => void copyRedirectUri(redirectInput));
  redirectRow.append(redirectInput, copyBtn);
  redirectLabel.append(redirectRow);
  body.append(redirectLabel);

  body.append(
    caption(
      '在飞书开放平台（open.feishu.cn）→ 开发者后台 → 自建应用的「凭证与基础信息」页获取。' +
        '「安全设置 → 重定向 URL」须与上方地址逐字一致。',
    ),
  );

  // Actions
  const actions = document.createElement('div');
  actions.className = 'actions';
  actions.append(saveBtn);
  if (f.source === 'keyring') {
    const clearBtn = document.createElement('button');
    clearBtn.type = 'button';
    clearBtn.className = 'link danger';
    clearBtn.textContent = '清除';
    clearBtn.disabled = feishuBusy;
    clearBtn.addEventListener('click', () => void feishuClearConfig());
    const spacer = document.createElement('span');
    spacer.className = 'spacer';
    actions.append(spacer, clearBtn);
  }
  body.append(actions);

  if (feishuStatus) {
    const status = document.createElement('p');
    status.className = feishuStatusIsError ? 'status error' : 'status';
    status.textContent = feishuStatus;
    body.append(status);
  }
  return body;
}

function feishuAuthSection(f: FeishuSettings): HTMLElement {
  const children: HTMLElement[] = [];

  if (f.authState === 'loggedIn') {
    const ok = document.createElement('p');
    ok.className = 'status ok';
    ok.textContent = '✓ 已登录';
    children.push(ok, caption(`租户：${f.tenantKey ?? '自建应用单租户'}`));
  } else if (f.authState === 'loggedOut') {
    children.push(caption('尚未登录。登录后即可在主窗口推送到飞书 / 从飞书拉取。'));
  } else {
    children.push(caption('先在上方保存飞书应用凭证，然后登录。'));
  }

  if (f.authState !== 'notConfigured') {
    const loggingIn = feishuLoggingIn || f.isLoggingIn;
    const actions = document.createElement('div');
    actions.className = 'actions';
    const loginBtn = document.createElement('button');
    loginBtn.type = 'button';
    loginBtn.className = 'primary';
    loginBtn.textContent = loggingIn
      ? '正在等待浏览器授权…'
      : f.authState === 'loggedIn'
        ? '重新登录'
        : '登录飞书';
    loginBtn.disabled = loggingIn || feishuBusy;
    loginBtn.addEventListener('click', () => void feishuLogin());
    actions.append(loginBtn);
    if (f.authState === 'loggedIn') {
      const spacer = document.createElement('span');
      spacer.className = 'spacer';
      const logoutBtn = document.createElement('button');
      logoutBtn.type = 'button';
      logoutBtn.className = 'link danger';
      logoutBtn.textContent = '登出';
      logoutBtn.disabled = loggingIn || feishuBusy;
      logoutBtn.addEventListener('click', () => void feishuLogout());
      actions.append(spacer, logoutBtn);
    }
    children.push(actions);
  }

  // 登录失败的详情走 payload.lastError(manager 记账),无需本地重复。
  if (f.lastError && f.authState !== 'loggedIn') {
    const err = document.createElement('p');
    err.className = 'status error';
    err.textContent = f.lastError;
    children.push(err);
  }

  return section('账号', ...children);
}

async function copyRedirectUri(input: HTMLInputElement): Promise<void> {
  const text = input.value.trim() || feishu?.defaultRedirectUri || '';
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // 自定义协议上下文里 Clipboard API 可能缺席 — 选区复制兜底。
    input.select();
    document.execCommand('copy');
  }
}

// MARK: - feishu actions

async function feishuSaveConfig(): Promise<void> {
  feishuBusy = true;
  feishuStatus = '';
  render();
  try {
    const result = await invoke<SaveResult>('feishu_save_app_config', {
      clientId: feishuDrafts.clientId,
      clientSecret: feishuDrafts.clientSecret,
      redirectUri: feishuDrafts.redirectUri,
    });
    if (result.ok) {
      feishuStatus = '已保存到凭据管理器。';
      feishuStatusIsError = false;
      feishuDrafts.clientSecret = '';
    } else {
      feishuStatus = result.error ?? '保存失败';
      feishuStatusIsError = true;
    }
  } catch (e) {
    feishuStatus = String(e);
    feishuStatusIsError = true;
  }
  feishuBusy = false;
  await refreshFeishu();
}

async function feishuClearConfig(): Promise<void> {
  feishuBusy = true;
  render();
  await invoke('feishu_clear_app_config');
  feishuBusy = false;
  await refreshFeishu();
}

async function feishuLogin(): Promise<void> {
  feishuLoggingIn = true;
  feishuStatus = '';
  render();
  try {
    // 失败详情由 manager.last_error 记账,refresh 后经 payload.lastError 显示。
    await invoke<SaveResult>('feishu_login');
  } catch (e) {
    feishuStatus = String(e);
    feishuStatusIsError = true;
  }
  feishuLoggingIn = false;
  await refreshFeishu();
}

async function feishuLogout(): Promise<void> {
  feishuBusy = true;
  render();
  await invoke('feishu_logout');
  feishuBusy = false;
  await refreshFeishu();
}

// MARK: - boot

async function boot(): Promise<void> {
  const app = document.getElementById('app');
  if (!app) return;
  if (!window.__TAURI__) {
    app.textContent = '请在 Done.md 应用内打开设置。';
    return;
  }
  settings = await invoke<SettingsPayload>('ai_settings_load');
  // DeepSeek expands by default (parity with the macOS panel).
  cardState('deepseek').expanded = true;
  render();
}

void boot();
