// The one modal primitive (PR gate F8, click-tests CT-F2 and N1). Every question that must be
// answered before the app goes on uses it; nothing else in the app declares aria-modal. While a
// question is open:
// - it is a <dialog> on <body>, outside <main>, opened with the platform's showModal() where the
//   webview has it, so a re-render of the app cannot remove it and the platform makes the rest
//   of the page inert as well;
// - everything else on <body> is inert too (for webviews without showModal), so no click or key
//   reaches the app behind it;
// - focus is only ever on one of the question's buttons: Tab and Shift+Tab are handled here for
//   every keydown that is a Tab (by key, code or keyCode; WebKitGTK sends Shift+Tab as
//   ISO_Left_Tab), and wrap between the buttons; the dialog box itself is not a Tab stop; focus
//   that lands anywhere else (the dialog box, the page behind, or nowhere) is put back on a
//   button;
// - the page behind does not scroll: the root's overflow is hidden, and keys that scroll
//   (arrows, Page Up/Down, Home, End, Space off a button) and the wheel are stopped;
// - Escape (a keydown, or the dialog's own cancel event) answers with `onCancel`.
// Closing removes it, lifts the inertness and the scroll lock it set, and puts focus where the
// caller says.

import { h } from "./dom";

export interface ModalOptions {
  /** Accessible title (a heading inside the dialog). */
  title: string;
  /** Accessible description (a paragraph under the title). */
  text: string;
  /** The answers, in order. The one marked `initial` gets focus when the question opens. */
  buttons: { label: string; primary?: boolean; initial?: boolean; onClick: () => void }[];
  /** Escape. */
  onCancel: () => void;
}

export interface Modal {
  readonly el: HTMLElement;
  /** Close and put focus on `focus` (if it is still in the page). Safe to call twice. */
  close(focus?: HTMLElement | null): void;
}

let open: Modal | null = null;

/** The question that is open, if any. */
export function openModal(): Modal | null {
  return open;
}

/** A Tab or Shift+Tab, however the engine names it. */
function isTab(e: KeyboardEvent): boolean {
  return e.key === "Tab" || e.code === "Tab" || e.keyCode === 9 || e.key === "ISO_Left_Tab";
}

/** Keys whose default action scrolls something. */
const SCROLL_KEYS = new Set(["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "PageUp", "PageDown", "Home", "End", " ", "Spacebar"]);

export function showModal(opts: ModalOptions): Modal {
  open?.close();
  const id = `modal-${Date.now().toString(36)}`;
  const buttons = opts.buttons.map((b) =>
    h("button", { type: "button", class: b.primary ? "primary" : null, onclick: () => b.onClick() }, b.label),
  );
  const dialog = h(
    "dialog",
    {
      class: "modal",
      role: "alertdialog",
      "aria-modal": "true",
      "aria-labelledby": `${id}-title`,
      "aria-describedby": `${id}-text`,
      // Focusable by script (a click on its text lands here and is sent on to a button), never
      // a Tab stop.
      tabindex: "-1",
    },
    h("h3", { id: `${id}-title` }, opts.title),
    h("p", { id: `${id}-text` }, opts.text),
    h("div", { class: "modal-buttons" }, ...buttons),
  ) as HTMLDialogElement;

  let closed = false;
  const usable = () => buttons.filter((b) => !b.disabled && b.isConnected);
  const first = () => usable()[0] ?? null;
  const last = () => {
    const list = usable();
    return list[list.length - 1] ?? null;
  };
  const onButton = (n: EventTarget | null) => n instanceof HTMLButtonElement && buttons.includes(n) && !n.disabled;

  /** Move focus one button on (or back), wrapping. */
  const step = (back: boolean) => {
    const list = usable();
    if (list.length === 0) return;
    const i = list.indexOf(document.activeElement as HTMLButtonElement);
    const next = i < 0 ? (back ? list.length - 1 : 0) : (i + (back ? -1 : 1) + list.length) % list.length;
    list[next]?.focus();
  };

  // Keys anywhere in the document while the question is open (capture: before the app sees them).
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape" || e.key === "Esc") {
      e.preventDefault();
      e.stopPropagation();
      opts.onCancel();
      return;
    }
    if (isTab(e)) {
      e.preventDefault();
      e.stopPropagation();
      step(e.shiftKey);
      return;
    }
    if (SCROLL_KEYS.has(e.key)) {
      // Space and Enter press the focused button; every other scroll key does nothing here.
      if ((e.key === " " || e.key === "Spacebar") && onButton(e.target)) return;
      e.preventDefault();
    }
  };
  // The dialog's own Escape handling closes it without our answer; answer instead.
  const onCancelEvent = (e: Event) => {
    e.preventDefault();
    opts.onCancel();
  };
  // Focus that lands anywhere but a button goes back to one: the button after the one it left,
  // in the direction it went (from the first button backwards means the last one).
  const onFocusIn = (e: FocusEvent) => {
    if (onButton(e.target)) return;
    const from = e.relatedTarget;
    (from === first() ? last() : first())?.focus();
  };
  // Focus that leaves a button for nowhere (no element takes it) comes back to that button.
  const onFocusOut = (e: FocusEvent) => {
    if (e.relatedTarget !== null || !onButton(e.target)) return;
    const left = e.target as HTMLButtonElement;
    queueMicrotask(() => {
      if (closed || onButton(document.activeElement)) return;
      (left.isConnected ? left : first())?.focus();
    });
  };
  const onWheel = (e: Event) => {
    const scrollable = dialog.scrollHeight > dialog.clientHeight;
    if (!(scrollable && dialog.contains(e.target as Node))) e.preventDefault();
  };

  // Inert everything else on <body>, remembering what this question set.
  const madeInert: Element[] = [];
  for (const el of Array.from(document.body.children)) {
    if (!el.hasAttribute("inert")) {
      el.setAttribute("inert", "");
      madeInert.push(el);
    }
  }
  // Lock the page's scroll, remembering what was there.
  const root = document.documentElement;
  const before = { root: root.style.overflow, body: document.body.style.overflow };
  root.style.overflow = "hidden";
  document.body.style.overflow = "hidden";

  document.body.append(dialog);
  dialog.addEventListener("cancel", onCancelEvent);
  document.addEventListener("keydown", onKey, true);
  document.addEventListener("focusin", onFocusIn, true);
  document.addEventListener("focusout", onFocusOut, true);
  document.addEventListener("wheel", onWheel, { capture: true, passive: false });
  document.addEventListener("touchmove", onWheel, { capture: true, passive: false });
  try {
    if (typeof dialog.showModal === "function") dialog.showModal();
    else dialog.setAttribute("open", "");
  } catch {
    dialog.setAttribute("open", "");
  }
  (buttons[opts.buttons.findIndex((b) => b.initial)] ?? first())?.focus();

  const modal: Modal = {
    el: dialog,
    close(focus) {
      if (closed) return;
      closed = true;
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("focusin", onFocusIn, true);
      document.removeEventListener("focusout", onFocusOut, true);
      document.removeEventListener("wheel", onWheel, true);
      document.removeEventListener("touchmove", onWheel, true);
      try {
        if (dialog.open && typeof dialog.close === "function") dialog.close();
      } catch {
        // Removed below either way.
      }
      dialog.remove();
      for (const el of madeInert) el.removeAttribute("inert");
      root.style.overflow = before.root;
      document.body.style.overflow = before.body;
      if (open === modal) open = null;
      if (focus?.isConnected) focus.focus();
    },
  };
  open = modal;
  return modal;
}
