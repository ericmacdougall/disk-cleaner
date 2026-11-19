// DOM building with text only: data (file names, paths, messages) is always set as text, never
// parsed as markup (spec AC-10). There is no innerHTML anywhere in this app.

type Attrs = Record<string, string | number | boolean | null | undefined | ((e: Event) => void)>;
type Child = Node | string | number | null | undefined | false;

/** Create an element. `on*` attributes become listeners; `false`/`null` attributes are left out. */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined || v === false) continue;
    if (typeof v === "function") {
      el.addEventListener(k.slice(2).toLowerCase(), v);
    } else if (k === "class") {
      el.className = String(v);
    } else if (k === "style") {
      // Through CSSOM: the CSP (style-src 'self') blocks style attributes, not CSSOM.
      el.style.cssText = String(v);
    } else if (v === true) {
      el.setAttribute(k, "");
    } else {
      el.setAttribute(k, String(v));
    }
  }
  append(el, ...children);
  return el;
}

/** Append children; strings become text nodes. */
export function append(el: Element, ...children: Child[]): void {
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    el.append(typeof c === "string" || typeof c === "number" ? document.createTextNode(String(c)) : c);
  }
}

/** Replace an element's children. */
export function replace(el: Element, ...children: Child[]): void {
  el.replaceChildren();
  append(el, ...children);
}

/** A polite live region message (status lines). */
export function announce(region: Element, text: string): void {
  region.textContent = text;
}
