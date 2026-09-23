// M4 end-to-end probe: HTTP Range support on the donemd-asset protocol.
// Uses Network domain on the visual target — CSP connect-src blocks fetch()
// to the asset host, but <video> loads go through media-src (allowed), and
// the Network domain sees every request incl. status + response headers.
// Also drives a real seek on the <video> element to prove 206-based seeking.

const list = await (await fetch('http://localhost:9222/json')).json();
const target = list.find(t => t.url.includes('visual.html'));
if (!target) { console.log('visual target not found'); process.exit(1); }
const ws = new WebSocket(target.webSocketDebuggerUrl);
let id = 0; const pending = new Map();
const assetResponses = [];
function call(method, params = {}) {
  return new Promise((resolve) => { const mid = ++id; pending.set(mid, resolve); ws.send(JSON.stringify({ id: mid, method, params })); });
}
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); return; }
  if (m.method === 'Network.responseReceived') {
    const r = m.params.response;
    if (r.url.includes('donemd-asset.localhost')) {
      assetResponses.push({
        url: r.url.split('/').pop(),
        status: r.status,
        contentRange: r.headers['Content-Range'] ?? r.headers['content-range'] ?? null,
        acceptRanges: r.headers['Accept-Ranges'] ?? r.headers['accept-ranges'] ?? null,
        contentType: r.headers['Content-Type'] ?? r.headers['content-type'] ?? null,
      });
    }
  }
};
await new Promise(r => { ws.onopen = r; });
const evalJs = async (expression) => {
  const res = await call('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (res.result?.exceptionDetails) return { error: res.result.exceptionDetails.text + ' ' + (res.result.exceptionDetails.exception?.description ?? '') };
  return res.result?.result?.value;
};

await call('Network.enable');

// 1) Wait for the video element to reach HAVE_METADATA (proves the first
//    range request(s) succeeded and the moov atom was parsed).
const meta = await evalJs(`(async () => {
  const v = document.querySelector('.ProseMirror video');
  if (!v) return { error: 'no <video> element — is the test doc open?' };
  if (v.readyState < 1) {
    await new Promise((res, rej) => {
      v.addEventListener('loadedmetadata', res, { once: true });
      v.addEventListener('error', () => rej(new Error('video error ' + v.error?.code)), { once: true });
      setTimeout(() => rej(new Error('timeout waiting metadata')), 8000);
    }).catch(e => ({ waitError: String(e) }));
  }
  return {
    src: v.src, readyState: v.readyState, duration: v.duration,
    seekable: v.seekable.length ? [v.seekable.start(0), v.seekable.end(0)] : null,
    videoSize: [v.videoWidth, v.videoHeight],
    error: v.error ? v.error.code : null,
  };
})()`);
console.log('metadata  :', JSON.stringify(meta));

// 2) Seek to mid-duration, await 'seeked' — the seek must trigger a new
//    ranged fetch and complete.
const seek = await evalJs(`(async () => {
  const v = document.querySelector('.ProseMirror video');
  if (!v || !v.duration) return { error: 'no duration' };
  const target = Math.max(0.1, v.duration * 0.6);
  v.currentTime = target;
  const ok = await new Promise((res) => {
    v.addEventListener('seeked', () => res(true), { once: true });
    setTimeout(() => res(false), 8000);
  });
  return { seeked: ok, landed: v.currentTime, target };
})()`);
console.log('seek      :', JSON.stringify(seek));

// 3) Report every asset-protocol response observed.
console.log('responses :', JSON.stringify(assetResponses, null, 1));
ws.close(); process.exit(0);
