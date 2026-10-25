/**
 * McQueen browser SDK — Layer 1 of the triage pipeline, on the frontend side.
 *
 * - Stamps a W3C `traceparent` on every API request (fetch and XHR) so one click
 *   in the browser becomes one trace id across launchpad → core → authrs → …
 * - When a request fails (5xx, chosen 4xx, network error) it posts the trace id
 *   plus what the browser saw to McQueen, and prints a DevTools link to the triage.
 *
 * No dependencies. Usage (client-ui-next, once, in a client component):
 *
 *   import { installMcQueen } from './mcqueen';
 *   installMcQueen({
 *     ingestUrl: 'https://triage.regere.ai/api/ingest/frontend',
 *     ingestKey: process.env.NEXT_PUBLIC_MCQUEEN_KEY,
 *     triageUrl: 'https://triage.regere.ai',
 *     propagateTo: [/^\/(api|proxy)\//, 'https://launchpad-headless-dev.regere.ai'],
 *   });
 */

export interface McQueenOptions {
  /** `POST` endpoint of McQueen: `<origin>/api/ingest/frontend`. */
  ingestUrl: string;
  /** Matches McQueen's INGEST_KEY (sent as `x-mcqueen-key`). */
  ingestKey?: string;
  /** McQueen UI origin, for console links. */
  triageUrl?: string;
  /**
   * Which request URLs get a `traceparent`. Strings match as URL prefixes, RegExps
   * test the full URL or path. Default: same-origin requests only.
   */
  propagateTo?: (string | RegExp)[];
  /** Report responses with status >= this. Default 500. */
  reportFrom?: number;
  /** Extra statuses to report (e.g. 403 to explain permission errors). */
  alsoReport?: number[];
  /** Never report requests whose URL matches (health checks, analytics…). */
  ignore?: (string | RegExp)[];
  appVersion?: string;
  /** Max response body characters sent along. Default 2000. */
  maxBody?: number;
  /** Log a line with the trace link in the console on failures. Default true. */
  console?: boolean;
}

export interface McQueenHandle {
  /** Trace id of the most recent instrumented request. */
  lastTraceId(): string | null;
  /** Report a UI-level failure (e.g. a field rendered `undefined`) against a trace. */
  reportUiError(message: string, traceId?: string | null): void;
  uninstall(): void;
}

const hex = (bytes: number) => {
  const a = new Uint8Array(bytes);
  crypto.getRandomValues(a);
  return Array.from(a, (b) => b.toString(16).padStart(2, '0')).join('');
};

const newTraceId = () => {
  let id = hex(16);
  while (/^0+$/.test(id)) id = hex(16);
  return id;
};

export const traceparentFor = (traceId: string) => `00-${traceId}-${hex(8)}-01`;

export function traceIdOf(traceparent: string | null | undefined): string | null {
  const m = traceparent?.trim().toLowerCase().match(/^[0-9a-f]{2}-([0-9a-f]{32})-[0-9a-f]{16}-[0-9a-f]{2}$/);
  return m && !/^0+$/.test(m[1]) ? m[1] : null;
}

const matches = (url: string, rules: (string | RegExp)[]) => {
  let path = url;
  try {
    path = new URL(url, location.href).pathname;
  } catch {
    /* relative or odd URL: keep as-is */
  }
  return rules.some((r) => (typeof r === 'string' ? url.startsWith(r) || path.startsWith(r) : r.test(url) || r.test(path)));
};

const sameOrigin = (url: string) => {
  try {
    return new URL(url, location.href).origin === location.origin;
  } catch {
    return false;
  }
};

const pathOnly = (url: string) => {
  try {
    const u = new URL(url, location.href);
    return u.origin === location.origin ? u.pathname : `${u.origin}${u.pathname}`;
  } catch {
    return url.split('?')[0];
  }
};

let installed: McQueenHandle | null = null;

export function installMcQueen(opts: McQueenOptions): McQueenHandle {
  if (installed) return installed;
  if (typeof window === 'undefined') {
    return { lastTraceId: () => null, reportUiError: () => {}, uninstall: () => {} };
  }

  const reportFrom = opts.reportFrom ?? 500;
  const also = new Set(opts.alsoReport ?? []);
  const maxBody = opts.maxBody ?? 2000;
  const logToConsole = opts.console ?? true;
  const ignore = [...(opts.ignore ?? []), opts.ingestUrl];
  let last: string | null = null;

  const originalFetch = window.fetch.bind(window);
  const OriginalXHR = window.XMLHttpRequest;

  const shouldPropagate = (url: string) =>
    !matches(url, [opts.ingestUrl]) && (opts.propagateTo ? matches(url, opts.propagateTo) : sameOrigin(url));
  const shouldReport = (status: number | null) => status === null || status >= reportFrom || also.has(status);

  const send = (payload: Record<string, unknown>) => {
    const headers: Record<string, string> = { 'content-type': 'application/json' };
    if (opts.ingestKey) headers['x-mcqueen-key'] = opts.ingestKey;
    originalFetch(opts.ingestUrl, { method: 'POST', headers, body: JSON.stringify(payload), keepalive: true, credentials: 'omit' })
      .then((r) => (r.ok ? r.json() : null))
      .then((res: { url?: string } | null) => {
        if (logToConsole && res?.url) console.info(`[McQueen] triage → ${res.url}`);
      })
      .catch(() => {
        /* reporting must never break the app */
      });
  };

  const report = (traceId: string, method: string, url: string, status: number | null, body: string | null, startedAt: number, uiMessage?: string) => {
    if (matches(url, ignore)) return;
    if (logToConsole) {
      const link = opts.triageUrl ? ` ${opts.triageUrl.replace(/\/$/, '')}/traces/${traceId}` : '';
      const what = uiMessage ? `UI error on ${location.pathname}: ${uiMessage}` : `${method} ${pathOnly(url)} → ${status ?? 'network error'}`;
      console.warn(`[McQueen] ${what} · trace ${traceId}${link}`);
    }
    send({
      trace_id: traceId,
      method,
      url: pathOnly(url),
      status,
      response_body: body ? body.slice(0, maxBody) : null,
      ui_message: uiMessage ?? null,
      page: location.pathname,
      app_version: opts.appVersion ?? null,
      duration_ms: Math.round(performance.now() - startedAt),
    });
  };

  // ── fetch ───────────────────────────────────────────────────────────────────
  window.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
    const method = (init?.method ?? (input instanceof Request ? input.method : 'GET')).toUpperCase();
    if (!shouldPropagate(url)) return originalFetch(input, init);

    const headers = new Headers(init?.headers ?? (input instanceof Request ? input.headers : undefined));
    let traceId = traceIdOf(headers.get('traceparent'));
    if (!traceId) {
      traceId = newTraceId();
      headers.set('traceparent', traceparentFor(traceId));
    }
    last = traceId;
    const startedAt = performance.now();
    // `init.headers` replaces a Request's own headers in fetch(), and `headers`
    // already merges both, so passing it via init covers every call shape.
    try {
      const res = await originalFetch(input, { ...init, headers });
      // Launchpad echoes the traceparent it used; prefer it if it differs.
      const echoed = traceIdOf(res.headers.get('traceparent')) ?? traceId;
      last = echoed;
      if (shouldReport(res.status)) {
        res
          .clone()
          .text()
          .then((body) => report(echoed, method, url, res.status, body, startedAt))
          .catch(() => report(echoed, method, url, res.status, null, startedAt));
      }
      return res;
    } catch (err) {
      if (!(err instanceof DOMException && err.name === 'AbortError')) {
        report(traceId, method, url, null, String(err), startedAt);
      }
      throw err;
    }
  };

  // ── XMLHttpRequest (axios & co.) ─────────────────────────────────────────────
  class TracedXHR extends OriginalXHR {
    private mq = { method: 'GET', url: '', traceId: '', startedAt: 0, traced: false };

    open(method: string, url: string | URL, async = true, username?: string | null, password?: string | null) {
      this.mq.method = method.toUpperCase();
      this.mq.url = String(url);
      super.open(method, url, async, username ?? null, password ?? null);
    }

    setRequestHeader(name: string, value: string) {
      if (name.toLowerCase() === 'traceparent') {
        const tid = traceIdOf(value);
        if (tid) {
          this.mq.traceId = tid;
          this.mq.traced = true;
        }
      }
      super.setRequestHeader(name, value);
    }

    send(body?: Document | XMLHttpRequestBodyInit | null) {
      if (shouldPropagate(this.mq.url)) {
        if (!this.mq.traced) {
          this.mq.traceId = newTraceId();
          super.setRequestHeader('traceparent', traceparentFor(this.mq.traceId));
        }
        last = this.mq.traceId;
        this.mq.startedAt = performance.now();
        this.addEventListener('loadend', () => {
          const status = this.status || null;
          const tid = traceIdOf(this.getResponseHeader('traceparent')) ?? this.mq.traceId;
          if (shouldReport(status)) {
            let text: string | null = null;
            try {
              text = this.responseType === '' || this.responseType === 'text' ? this.responseText : null;
            } catch {
              text = null;
            }
            report(tid, this.mq.method, this.mq.url, status, text, this.mq.startedAt);
          }
        });
      }
      super.send(body);
    }
  }
  window.XMLHttpRequest = TracedXHR as unknown as typeof XMLHttpRequest;

  installed = {
    lastTraceId: () => last,
    reportUiError: (message, traceId) => {
      const tid = traceId ?? last;
      if (tid) report(tid, 'UI', location.pathname, null, null, performance.now(), message);
    },
    uninstall: () => {
      window.fetch = originalFetch;
      window.XMLHttpRequest = OriginalXHR;
      installed = null;
    },
  };
  return installed;
}
