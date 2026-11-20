// A small W3C WebDriver client over fetch (Node 22), enough to drive the packaged app through
// tauri-driver: sessions, elements, clicks, keys and scripts. No dependencies.

const ELEMENT = "element-6066-11e4-a52f-4a6a43c3c0d1";

export class WebDriverError extends Error {
  constructor(what, value) {
    super(`${what}: ${value?.error ?? "error"}: ${value?.message ?? JSON.stringify(value)}`);
    this.value = value;
  }
}

export class Session {
  constructor(base, id) {
    this.base = base;
    this.id = id;
  }

  static async start(base, capabilities, timeoutMs = 120_000) {
    const body = JSON.stringify({ capabilities: { alwaysMatch: capabilities } });
    const res = await fetch(`${base}/session`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body,
      signal: AbortSignal.timeout(timeoutMs),
    });
    const json = await res.json();
    if (!res.ok) throw new WebDriverError("new session", json.value);
    return new Session(base, json.value.sessionId);
  }

  async cmd(method, path, body, timeoutMs = 60_000) {
    const res = await fetch(`${this.base}/session/${this.id}${path}`, {
      method,
      headers: { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(timeoutMs),
    });
    const json = await res.json();
    if (!res.ok) throw new WebDriverError(`${method} ${path}`, json.value);
    return json.value;
  }

  async end() {
    try {
      await this.cmd("DELETE", "", undefined, 30_000);
    } catch {
      // The app may already be gone.
    }
  }

  /** Run `body` (a function body; `arguments` holds args) in the page. */
  exec(body, ...args) {
    return this.cmd("POST", "/execute/sync", { script: body, args });
  }

  /** Run `body` in the page; it calls the last argument with its result. */
  execAsync(body, timeoutMs, ...args) {
    return this.cmd("POST", "/execute/async", { script: body, args }, timeoutMs + 10_000);
  }

  async find(css) {
    const v = await this.cmd("POST", "/element", { using: "css selector", value: css });
    return v[ELEMENT];
  }

  async findAll(css) {
    const v = await this.cmd("POST", "/elements", { using: "css selector", value: css });
    return v.map((e) => e[ELEMENT]);
  }

  /** The first element matching `css` whose trimmed text is `text` (or starts with it, when
   * `prefix`); waits up to `timeoutMs`. */
  async findText(css, text, { prefix = false, timeoutMs = 30_000 } = {}) {
    const v = await this.until(
      `const [css, text, prefix] = arguments;
       return [...document.querySelectorAll(css)].find((e) => {
         const t = e.textContent.trim();
         return prefix ? t.startsWith(text) : t === text;
       }) ?? null;`,
      timeoutMs,
      css,
      text,
      prefix,
    );
    return v[ELEMENT];
  }

  click(el) {
    return this.cmd("POST", `/element/${el}/click`, {});
  }

  /** Click a button by its text (waits for it, and for it to be enabled). */
  async clickText(text, css = "button", opts = {}) {
    const el = await this.findText(`${css}:not([disabled])`, text, opts);
    await this.click(el);
    return el;
  }

  dblclick(el) {
    const press = [
      { type: "pointerDown", button: 0 },
      { type: "pointerUp", button: 0 },
    ];
    return this.cmd("POST", "/actions", {
      actions: [
        {
          type: "pointer",
          id: "mouse",
          parameters: { pointerType: "mouse" },
          actions: [{ type: "pointerMove", origin: { [ELEMENT]: el }, x: 0, y: 0 }, ...press, ...press],
        },
      ],
    });
  }

  text(el) {
    return this.cmd("GET", `/element/${el}/text`);
  }

  /** Press keys (WebDriver key codes, e.g. "" for Tab) on the focused element. */
  keys(...keys) {
    const actions = keys.flatMap((k) => [
      { type: "keyDown", value: k },
      { type: "keyUp", value: k },
    ]);
    return this.cmd("POST", "/actions", { actions: [{ type: "key", id: "keyboard", actions }] });
  }

  /** Wait until `body` (a sync script) returns a truthy value; return it. */
  async until(body, timeoutMs = 30_000, ...args) {
    const end = Date.now() + timeoutMs;
    let last;
    for (;;) {
      try {
        last = await this.exec(body, ...args);
        if (last) return last;
      } catch (e) {
        last = e;
      }
      if (Date.now() > end) throw new Error(`timed out waiting for: ${body.slice(0, 120)} (last: ${last instanceof Error ? last.message : JSON.stringify(last)})`);
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

export const KEY = { tab: "", enter: "", escape: "", down: "", up: "", right: "", left: "", end: "", home: "", shift: "" };
