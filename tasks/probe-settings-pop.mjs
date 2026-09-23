// Settings model-dropdown probe (fixed-position popup fix).
// Opens the settings window via the bridge envelope from the main webview,
// injects a 12-model list into the largest select (the saved config may hold
// fewer models than the live fetch shows), then measures the popup.

const base = 'http://localhost:9222';
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

function driver(wsUrl) {
  return new Promise(async (resolveDriver) => {
    const ws = new WebSocket(wsUrl);
    let id = 0;
    const pending = new Map();
    ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
    };
    await new Promise((r) => { ws.onopen = r; });
    const call = (method, params = {}) => new Promise((resolve) => {
      const mid = ++id; pending.set(mid, resolve);
      ws.send(JSON.stringify({ id: mid, method, params }));
    });
    const evalJs = async (expression) => {
      const res = await call('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
      if (res.result?.exceptionDetails) {
        return { error: res.result.exceptionDetails.text + ' :: ' + (res.result.exceptionDetails.exception?.description ?? '') };
      }
      return res.result?.result?.value;
    };
    resolveDriver({ ws, call, evalJs });
  });
}

// 1) Find the main visual target, open the settings window via bridge.
let targets = await (await fetch(`${base}/json`)).json();
const settings0 = targets.find((t) => t.url.includes('settings.html'));
let settings = settings0;
if (!settings) {
  const main = targets.find((t) => t.url.includes('visual.html'));
  if (!main) { console.log('FAIL: no visual.html target'); process.exit(1); }
  const mainD = await driver(main.webSocketDebuggerUrl);
  await mainD.evalJs(`window.__TAURI__.core.invoke('bridge_dispatch', { envelope: { version: 1, type: 'aiOpenSettings', payload: {} } })`);
  for (let i = 0; i < 40 && !settings; i++) {
    await wait(250);
    targets = await (await fetch(`${base}/json`)).json();
    settings = targets.find((t) => t.url.includes('settings.html'));
  }
  mainD.ws.close();
}
if (!settings) { console.log('FAIL: settings window never appeared'); process.exit(1); }
const d = await driver(settings.webSocketDebuggerUrl);
await wait(400);

// 2) Expand all cards, inject fake models into the LARGEST model select to
//    simulate an 8+ model list, open it, measure.
const r1 = await d.evalJs(`(() => {
  document.querySelectorAll('.card-header').forEach((h) => {
    if (h.querySelector('.chevron') && h.querySelector('.chevron').textContent !== '▾') h.click();
  });
  const pops = [...document.querySelectorAll('.xselect')].map((x, i) => ({
    root: x, i,
    count: x.querySelectorAll('.xselect-opt').length,
  }));
  const best = pops.sort((a, b) => b.count - a.count)[0];
  // Grow to 12 models (the "8+" scenario from the user report).
  const pop = best.root.querySelector('.xselect-pop');
  const real = pop.querySelectorAll('.xselect-opt').length;
  for (let k = real; k < 12; k++) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'xselect-opt';
    b.textContent = 'model-injected-' + (k + 1);
    pop.append(b);
  }
  return { perSelect: pops.map((p) => p.count), injectedTo: 12, chosenIndex: best.i };
})()`);
console.log('step1:', JSON.stringify(r1));

const r2 = await d.evalJs(`(() => {
  const pops = [...document.querySelectorAll('.xselect')].map((x) => ({
    root: x,
    count: x.querySelectorAll('.xselect-opt').length,
  }));
  const best = pops.sort((a, b) => b.count - a.count)[0];
  const face = best.root.querySelector('.xselect-face');
  // Scroll so the face sits in the LOWER third of the viewport — the worst
  // case for a downward popup (this is where the old clipping bit hardest).
  face.scrollIntoView({ block: 'end' });
  face.click();
  const pop = best.root.querySelector('.xselect-pop');
  const cs = getComputedStyle(pop);
  const pr = pop.getBoundingClientRect();
  const fr = face.getBoundingClientRect();
  const card = best.root.closest('.card');
  const cr = card.getBoundingClientRect();
  const opts = [...pop.querySelectorAll('.xselect-opt')];
  const selected = pop.querySelector('.xselect-opt.selected');
  const last = opts[opts.length - 1];
  pop.scrollTop = pop.scrollHeight;
  const lastReachable = last.offsetTop + last.offsetHeight - pop.scrollTop <= pop.clientHeight + 0.5;
  return {
    optionCount: opts.length,
    faceInViewport: fr.top >= 0 && fr.bottom <= window.innerHeight,
    face: { y: Math.round(fr.top), bottom: Math.round(fr.bottom) },
    pop: { x: Math.round(pr.x), y: Math.round(pr.y), w: Math.round(pr.width), h: Math.round(pr.height) },
    cardBottom: Math.round(cr.bottom),
    cardTop: Math.round(cr.top),
    computed: { position: cs.position, overflowY: cs.overflowY },
    inline: { top: pop.style.top, bottom: pop.style.bottom, maxHeight: pop.style.maxHeight },
    viewportH: window.innerHeight,
    scrollable: pop.scrollHeight > pop.clientHeight,
    scrollRangePx: pop.scrollHeight - pop.clientHeight,
    // The fix's whole point: escape the card's overflow:hidden clipping.
    extendsPastCardBottom: pr.bottom > cr.bottom + 0.5,
    fitsViewport: pr.top >= -0.5 && pr.bottom <= window.innerHeight + 0.5,
    selectedVisibleOnOpen: selected ? selected.offsetTop - pop.scrollTop < pop.clientHeight : null,
    lastReachableAfterFullScroll: lastReachable,
  };
})()`);
console.log('step2 (12 models, face near viewport bottom):');
console.log(JSON.stringify(r2, null, 1));

// 3) Close (the r2 click left it open), reopen, page-scroll must close it.
const r3 = await d.evalJs(`(async () => {
  const pops = [...document.querySelectorAll('.xselect')].map((x) => ({
    root: x,
    count: x.querySelectorAll('.xselect-opt').length,
  }));
  const best = pops.sort((a, b) => b.count - a.count)[0];
  const face = best.root.querySelector('.xselect-face');
  const pop = best.root.querySelector('.xselect-pop');
  if (!pop.hidden) face.click();           // close r2's open state
  face.click();                            // reopen
  if (pop.hidden) return { error: 'did not reopen' };
  window.scrollTo(0, 60);
  await new Promise((r) => setTimeout(r, 120));
  const closedOnPageScroll = pop.hidden;
  face.click();                            // reopen again
  const reopened = !pop.hidden;
  // Scrolling INSIDE the popup must keep it open.
  pop.scrollTop = 40;
  await new Promise((r) => setTimeout(r, 120));
  const stayedOpenOnInnerScroll = !pop.hidden;
  window.scrollTo(0, 0);
  return { closedOnPageScroll, reopened, stayedOpenOnInnerScroll };
})()`);
console.log('step3 scroll behavior :', JSON.stringify(r3));

d.ws.close(); process.exit(0);
