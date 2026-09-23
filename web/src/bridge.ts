// Bridge client. Mirrors the Swift WebViewBridge envelope shape:
//   { version: 1, type: string, payload: any }

export interface BridgeEnvelope {
  version: number;
  type: string;
  payload: unknown;
}

const ENVELOPE_VERSION = 1;
const handlers: Record<string, (payload: unknown) => void> = {};

declare global {
  interface Window {
    webkit?: {
      messageHandlers?: {
        donemd?: { postMessage: (msg: string) => void };
      };
    };
    /** Present when running inside a Tauri webview (withGlobalTauri). */
    __TAURI__?: {
      core: { invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown> };
      event: {
        listen: (
          event: string,
          cb: (e: { payload: unknown }) => void,
        ) => Promise<() => void>;
      };
    };
    donemdBridge: {
      receive: (envelope: BridgeEnvelope) => void;
    };
  }
}

/** Register a handler for inbound (Swift → JS) messages of the given type. */
export function on(type: string, handler: (payload: unknown) => void): void {
  handlers[type] = handler;
}

// Transport selection: inside Tauri (Windows shell) the identical envelope
// rides `invoke('bridge_dispatch')` outbound and the `bridge` event inbound;
// on macOS it uses webkit.messageHandlers as before. Everything above the
// transport (envelope shape, request/response correlation) is shared.
const tauri = window.__TAURI__;

/** True inside the Tauri (Windows) shell. */
export const IS_TAURI = !!tauri;

/** Modifier-key label for user-visible shortcut hints: '⌘' on macOS,
 *  'Ctrl+' on Windows (joining conventions differ: ⌘/ vs Ctrl+/). */
export const MOD = IS_TAURI ? 'Ctrl+' : '⌘';

/** Option/Alt label for shortcut hints: '⌥' on macOS, 'Alt+' on Windows. */
export const ALT = IS_TAURI ? 'Alt+' : '⌥';

/**
 * Resolves once the Tauri `bridge` event listener is truly live (the JS-side
 * dispatch map is populated only after the listen invoke round-trips). Pages
 * MUST await this before announcing `editorReady`: native answers ready with
 * an immediate snapshot (loadDocument / outlineSet), and a small page can
 * finish module eval before the listener exists — the reply is then dropped
 * silently. (Bit us on the 4 KB outline page; the 4.6 MB visual page always
 * won the race by accident.)
 *
 * On non-Tauri transports there is no async registration — already ready.
 */
export const bridgeReady: Promise<unknown> = tauri
  ? tauri.event.listen('bridge', (e) => {
      window.donemdBridge.receive(e.payload as BridgeEnvelope);
    })
  : Promise.resolve();

/** Send a message from JS to the native side. No-op (with a warning) when
 *  running in a plain browser (e.g. `npm run dev`) — useful for previewing
 *  the editor. */
export function send(type: string, payload: unknown = null): void {
  const envelope: BridgeEnvelope = { version: ENVELOPE_VERSION, type, payload };
  if (tauri) {
    tauri.core.invoke('bridge_dispatch', { envelope }).catch((e) => {
      console.warn('[bridge] tauri invoke failed:', e);
    });
    return;
  }
  const handler = window.webkit?.messageHandlers?.donemd;
  if (handler) {
    handler.postMessage(JSON.stringify(envelope));
  } else {
    console.info('[bridge] not in WKWebView; dropped:', envelope);
  }
}

// Expose the inbound receive() function on `window` so Swift can call it via
// evaluateJavaScript("window.donemdBridge.receive({...})").
// Request/response support for messages where JS needs to wait for Swift's
// answer (e.g. "import these image bytes and tell me the asset URL").
type Pending = (payload: unknown) => void;
const pending = new Map<string, Pending>();
let nextRequestId = 1;

/** Send `type` with `payload` and resolve when Swift sends a reply of
 *  `replyType` carrying the same `requestId`. */
export function request(
  type: string,
  payload: Record<string, unknown>,
  replyType: string,
): Promise<Record<string, unknown>> {
  return new Promise((resolve, reject) => {
    const requestId = `req-${nextRequestId++}`;
    pending.set(requestId, (p) => resolve(p as Record<string, unknown>));
    // Make sure the reply handler is wired exactly once.
    if (!handlers[replyType]) {
      on(replyType, (replyPayload) => {
        const obj = replyPayload as Record<string, unknown>;
        const id = obj?.requestId as string | undefined;
        if (id && pending.has(id)) {
          const resolver = pending.get(id)!;
          pending.delete(id);
          resolver(obj);
        }
      });
    }
    try {
      send(type, { ...payload, requestId });
    } catch (e) {
      pending.delete(requestId);
      reject(e);
    }
  });
}

window.donemdBridge = {
  receive(envelope: BridgeEnvelope): void {
    if (envelope.version !== ENVELOPE_VERSION) {
      console.warn('[bridge] unsupported envelope version:', envelope.version);
      return;
    }
    const handler = handlers[envelope.type];
    if (handler) {
      handler(envelope.payload);
    } else {
      console.warn('[bridge] no handler for type:', envelope.type);
    }
  },
};
