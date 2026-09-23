// 文档大纲边栏 — Windows (Tauri) counterpart of macOS's native
// `OutlineSidebar` (SwiftUI List in a NavigationSplitView column).
//
// The Windows shell is multi-webview: this page IS the sidebar, hosted in its
// own child webview left of the Visual pane. It is a pure projection — the
// Visual pane (main.ts) pushes heading data via the native relay, and row
// clicks go back through native which fans `scrollToHeading` out to BOTH the
// Visual and the Markdown-source panes (heading-ordinal anchored, same as
// macOS).
//
// Inbound : outlineSet {headings:[{level,text,index}]}  — full replacement
//           outlineActive {index|null}                  — scrollspy highlight
// Outbound: editorReady                                 — request initial state
//           outlineJump {index}                         — row click

import './outline.css';
import { bridgeReady, on, send } from './bridge';

interface OutlineHeading {
  level: number;
  text: string;
  index: number;
}

let headings: OutlineHeading[] = [];
let activeIndex: number | null = null;

const mount = document.getElementById('outline');
if (!mount) throw new Error('outline mount point #outline not found');

function render(): void {
  if (!mount) return;
  mount.textContent = '';
  if (headings.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'outline-empty';
    empty.textContent = '无标题';
    mount.appendChild(empty);
    return;
  }
  const list = document.createElement('div');
  list.className = 'outline-list';
  list.setAttribute('role', 'tree');
  for (const h of headings) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className =
      'outline-row' +
      ` outline-l${Math.min(h.level, 6)}` +
      (h.index === activeIndex ? ' active' : '');
    row.style.paddingLeft = `${(h.level - 1) * 12 + 10}px`;
    row.textContent = h.text;
    row.title = h.text;
    row.dataset.index = String(h.index);
    row.addEventListener('click', () => {
      // Optimistic highlight (macOS pendingIndex); the scrollspy's
      // outlineActive takes over once the panes land.
      setActive(h.index, false);
      send('outlineJump', { index: h.index });
    });
    list.appendChild(row);
  }
  mount.appendChild(list);
}

function setActive(index: number | null, scroll: boolean): void {
  if (index === activeIndex) return;
  activeIndex = index;
  if (!mount) return;
  const rows = mount.querySelectorAll<HTMLElement>('.outline-row');
  let activeEl: HTMLElement | null = null;
  for (const row of Array.from(rows)) {
    const isActive = row.dataset.index === String(index);
    row.classList.toggle('active', isActive);
    if (isActive) activeEl = row;
  }
  if (scroll && activeEl) {
    activeEl.scrollIntoView({ block: 'nearest' });
  }
}

on('outlineSet', (payload) => {
  const raw = (payload as { headings?: unknown } | null)?.headings;
  headings = Array.isArray(raw)
    ? raw
        .filter(
          (h): h is OutlineHeading =>
            typeof h === 'object' &&
            h !== null &&
            typeof (h as OutlineHeading).level === 'number' &&
            typeof (h as OutlineHeading).index === 'number' &&
            typeof (h as OutlineHeading).text === 'string',
        )
        .map((h) => ({ level: h.level, text: h.text, index: h.index }))
    : [];
  render();
  // Re-apply the current highlight against the fresh rows.
  const idx = activeIndex;
  activeIndex = null;
  setActive(idx, false);
});

on('outlineActive', (payload) => {
  const raw = (payload as { index?: unknown } | null)?.index;
  setActive(typeof raw === 'number' ? raw : null, true);
});

// Announce readiness only once the bridge listener is actually live (see
// bridgeReady): native answers editorReady with the outlineSet/outlineActive
// snapshot, and this tiny page would otherwise finish eval before the
// listener exists and silently drop the snapshot (black empty sidebar).
void bridgeReady.then(() => send('editorReady'));
