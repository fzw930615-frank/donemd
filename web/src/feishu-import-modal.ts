/**
 * 「从飞书链接新建」URL 录入弹窗(M8 F4-b)。
 *
 * 为什么做在前端而不是原生对话框:① `tauri-plugin-dialog` 没有文本输入
 * 对话框;② 这个应用自己的 UI 语言在 web 层(斜杠菜单 / 气泡菜单 /
 * Mermaid 抽屉),原生 Win32 弹框与那套毛玻璃界面割裂。
 *
 * 结构与关闭语义刻意对齐 `mermaid-edit-drawer`:透明遮罩层只做点击捕获,
 * 面板自带毛玻璃;点遮罩 / Esc / 取消 都关闭,Enter 直接提交。
 *
 * 提交后经 `feishuImportFromUrl` 信封交给原生,由它解析 token、拉取、
 * 再弹保存位置对话框。前端只负责收一个字符串 —— token 合法性判定属于
 * 原生侧的 `feishu::url::extract_doc_token`,不在两处重复实现。
 */
import { send } from './bridge';

let active: { close: () => void } | null = null;

/** 看着像飞书文档链接吗?仅用于剪贴板预填的启发式判断,不做校验。 */
function looksLikeFeishuDocUrl(text: string): boolean {
  const t = text.trim();
  if (t.length > 500 || !/^https?:\/\//i.test(t)) return false;
  return /\/(docx|wiki|docs)\//i.test(t);
}

export function openFeishuImportModal(): void {
  // 重复触发(菜单连点)时把上一个收掉,不叠层。
  if (active) active.close();

  // 记住开弹窗前的焦点归属 —— 通常是编辑器的 contenteditable。关闭时
  // 必须还回去:焦点留在 body 上会让 Tiptap 的全部快捷键(Ctrl+B/I/…)
  // 静默失效,表现为「所有快捷键都坏了」(2026-09-23 实测)。
  const previouslyFocused = document.activeElement as HTMLElement | null;

  const overlay = document.createElement('div');
  overlay.className = 'donemd-feishu-modal-overlay';

  const panel = document.createElement('div');
  panel.className = 'donemd-feishu-modal';
  overlay.appendChild(panel);

  const title = document.createElement('div');
  title.className = 'donemd-feishu-modal__title';
  title.textContent = '从飞书链接新建';
  panel.appendChild(title);

  const hint = document.createElement('div');
  hint.className = 'donemd-feishu-modal__hint';
  hint.textContent = '粘贴飞书云文档链接，将拉取内容并保存为本地文件。';
  panel.appendChild(hint);

  const input = document.createElement('input');
  input.className = 'donemd-feishu-modal__input';
  input.type = 'text';
  input.spellcheck = false;
  input.placeholder = 'https://你的租户.feishu.cn/wiki/…';
  input.setAttribute('aria-label', '飞书文档链接');
  panel.appendChild(input);

  const error = document.createElement('div');
  error.className = 'donemd-feishu-modal__error';
  panel.appendChild(error);

  const actions = document.createElement('div');
  actions.className = 'donemd-feishu-modal__actions';
  const cancelBtn = document.createElement('button');
  cancelBtn.type = 'button';
  cancelBtn.className = 'donemd-feishu-modal__btn';
  cancelBtn.textContent = '取消';
  const okBtn = document.createElement('button');
  okBtn.type = 'button';
  okBtn.className = 'donemd-feishu-modal__btn donemd-feishu-modal__btn--primary';
  okBtn.textContent = '新建';
  actions.appendChild(cancelBtn);
  actions.appendChild(okBtn);
  panel.appendChild(actions);

  document.body.appendChild(overlay);

  function close(): void {
    document.removeEventListener('keydown', onKeyDown, true);
    overlay.remove();
    active = null;
    // 焦点还给编辑器(见 previouslyFocused 的注释)。元素可能已不在文档里
    // (文档被替换过),所以静默容错。
    try {
      previouslyFocused?.focus();
    } catch {
      /* 元素已消失,交给 loadDocument 的聚焦兜底 */
    }
  }

  function submit(): void {
    const value = input.value.trim();
    if (!value) {
      // 空输入就地提示,不劳原生往返一趟。
      error.textContent = '请先粘贴链接。';
      input.focus();
      return;
    }
    send('feishuImportFromUrl', { url: value });
    close();
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      close();
    } else if (e.key === 'Enter' && document.activeElement === input) {
      e.preventDefault();
      submit();
    }
  }

  // 捕获阶段监听:Esc 不能漏给编辑器(那里 Esc 有取消 AI 流等语义)。
  document.addEventListener('keydown', onKeyDown, true);
  cancelBtn.addEventListener('click', close);
  okBtn.addEventListener('click', submit);
  overlay.addEventListener('mousedown', (e) => {
    if (e.target === overlay) close();
  });
  // 输入即清掉上一次的错误提示。
  input.addEventListener('input', () => {
    error.textContent = '';
  });

  active = { close };
  input.focus();

  // 用户十有八九刚在飞书里复制了链接 —— 命中就预填并全选,一次回车即走。
  // 剪贴板在 WebView2 下可能被拒或需要手势,失败就当没这回事。
  void (async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (!input.value && looksLikeFeishuDocUrl(text)) {
        input.value = text.trim();
        input.select();
      }
    } catch {
      /* 读不到剪贴板不影响手动粘贴 */
    }
  })();
}
