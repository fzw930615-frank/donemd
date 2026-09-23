// One-shot: invoke aiOpenSettings (envelope v1) from the visual page and
// watch what target appears + whether the invoke promise ever resolves.
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
    const evalJs = async (expression, awaitPromise = true) => {
      const res = await call('Runtime.evaluate', { expression, returnByValue: true, awaitPromise });
      if (res.result?.exceptionDetails) {
        return { error: res.result.exceptionDetails.text + ' :: ' + (res.result.exceptionDetails.exception?.description ?? '') };
      }
      return res.result?.result?.value;
    };
    resolveDriver({ ws, call, evalJs });
  });
}

const targets = () => fetch('http://localhost:9222/json').then((r) => r.json());

let ts = await targets();
const main = ts.find((t) => t.url.includes('visual.html'));
if (!main) { console.log('FAIL no visual target'); process.exit(1); }
console.log('step1: connected to', main.url);
const d = await driver(main.webSocketDebuggerUrl);
console.log('step2: ws ready');

// Fire the invoke WITHOUT awaiting the JS promise (awaitPromise false) so a
// hung command can't hang this probe.
const fired = await d.evalJs(`(() => {
  window.__lastSettle = 'pending';
  window.__TAURI__.core.invoke('bridge_dispatch', { envelope: { version: 1, type: 'aiOpenSettings', payload: {} } })
    .then(() => { window.__lastSettle = 'resolved'; }, (e) => { window.__lastSettle = 'rejected:' + e; });
  return 'fired';
})()`, false);
console.log('step3: invoke fired, js value =', JSON.stringify(fired));

for (let i = 0; i < 24; i++) {
  await wait(500);
  ts = await targets();
  const interesting = ts.map((t) => t.url).filter((u) => !u.includes('visual.html') && !u.includes('markdown-source.html'));
  if (interesting.length) {
    console.log(`step4 (t+${(i + 1) * 0.5}s): new targets:`, JSON.stringify(interesting));
    if (interesting.some((u) => u.includes('settings.html'))) {
      console.log('SUCCESS: settings window loaded settings.html');
      break;
    }
    if (i === 23) console.log('TIMEOUT: settings window never navigated past', JSON.stringify(interesting));
  }
}

// Did the invoke promise settle? (checks main-thread liveness indirectly)
const settled = await d.evalJs(`window.__lastSettle ?? 'pending'`, true).catch(() => 'eval-failed');
console.log('step5: ', settled);

// Is the visual page still responsive?
const alive = await d.evalJs(`document.title`, true).catch(() => 'eval-failed');
console.log('step6: visual page alive, title =', JSON.stringify(alive));

d.ws.close();
process.exit(0);
