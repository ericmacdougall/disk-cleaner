// The one modal primitive (PR gate F8). Every question that must be answered before the app goes
// on uses it; nothing else in the app declares aria-modal. While a question is open:
// - it is a <dialog> on <body>, outside <main>, so a re-render of the app cannot remove it;
// - everything else on <body> is inert (and, where the webview has it, showModal() makes the
//   rest of the page inert as well), so no click or key reaches the app behind it;
// - Tab and Shift+Tab cycle through the question's own buttons, and focus that lands outside
//   (a script, a re-render) is brought back to it;
// - Escape (a keydown, or the dialog's own cancel event) answers with `onCancel`.
// Closing removes it, lifts the inertness it set, and puts focus where the caller says.

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
    },
    h("h3", { id: `${id}-title` }, opts.title),
    h("p", { id: `${id}-text` }, opts.text),
    h("div", { class: "modal-buttons" }, ...buttons),
  ) as HTMLDialogElement;

  const first = () => buttons.find((b) => !b.disabled) ?? null;
  const last = () => [...buttons].reverse().find((b) => !b.disabled) ?? null;
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      opts.onCancel();
      return;
    }
    if (e.key !== "Tab") return;
    const active = document.activeElement;
    const target = !dialog.contains(active) ? first() : e.shiftKey ? (active === first() ? last() : null) : active === last() ? first() : null;
    if (target) {
      e.preventDefault();
      target.focus();
    }
  };
  // The dialog's own Escape handling closes it without our answer; answer instead.
  const onCancelEvent = (e: Event) => {
    e.preventDefault();
    opts.onCancel();
  };
  const onFocusIn = (e: FocusEvent) => {
    if (!dialog.contains(e.target as Node)) first()?.focus();
  };

  // Inert everything else on <body>, remembering what this question set.
  const madeInert: Element[] = [];
  for (const el of Array.from(document.body.children)) {
    if (!el.hasAttribute("inert")) {
      el.setAttribute("inert", "");
      madeInert.push(el);
    }
  }
  document.body.append(dialog);
  dialog.addEventListener("keydown", onKey);
  dialog.addEventListener("cancel", onCancelEvent);
  document.addEventListener("focusin", onFocusIn, true);
  try {
    if (typeof dialog.showModal === "function") dialog.showModal();
    else dialog.setAttribute("open", "");
  } catch {
    dialog.setAttribute("open", "");
  }
  (buttons[opts.buttons.findIndex((b) => b.initial)] ?? first())?.focus();

  let closed = false;
  const modal: Modal = {
    el: dialog,
    close(focus) {
      if (closed) return;
      closed = true;
      document.removeEventListener("focusin", onFocusIn, true);
      try {
        if (dialog.open && typeof dialog.close === "function") dialog.close();
      } catch {
        // Removed below either way.
      }
      dialog.remove();
      for (const el of madeInert) el.removeAttribute("inert");
      if (open === modal) open = null;
      if (focus?.isConnected) focus.focus();
    },
  };
  open = modal;
  return modal;
}
