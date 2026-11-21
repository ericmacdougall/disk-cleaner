// The modal primitive keeps focus on its buttons and the page behind still (click-test N1: in
// the shipped AppImage Shift+Tab left the question for the dialog box and then for nowhere, and
// arrows, Space, Home/End and the wheel scrolled the page behind it).

import { afterEach, describe, expect, it, vi } from "vitest";
import { openModal, showModal } from "./modal";

function setup(extra: Element[] = []) {
  const behind = document.createElement("main");
  const field = document.createElement("input");
  const other = document.createElement("button");
  other.textContent = "New scan";
  behind.append(field, other);
  document.body.replaceChildren(behind, ...extra);
  field.focus();
  const answers: string[] = [];
  const m = showModal({
    title: "Shorten the active window?",
    text: "shortening the active window from 30 to 7 days",
    buttons: [
      { label: "Keep 30 days", initial: true, onClick: () => answers.push("keep") },
      { label: "Shorten to 7 days", primary: true, onClick: () => answers.push("shorten") },
    ],
    onCancel: () => answers.push("cancel"),
  });
  const [keep, shorten] = [...m.el.querySelectorAll("button")] as HTMLButtonElement[];
  return { m, keep: keep!, shorten: shorten!, behind, field, other, answers };
}

function key(target: EventTarget, init: KeyboardEventInit & { keyCode?: number }): KeyboardEvent {
  const e = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
  if (init.keyCode !== undefined) Object.defineProperty(e, "keyCode", { value: init.keyCode });
  target.dispatchEvent(e);
  return e;
}
const tick = () => new Promise<void>((r) => queueMicrotask(r));

afterEach(() => {
  openModal()?.close();
  vi.restoreAllMocks();
});

describe("the modal question keeps focus on its buttons (click-test N1)", () => {
  // WebKitGTK reports Shift+Tab from a real keyboard as ISO_Left_Tab; other engines differ in key
  // and code. Every form must wrap from the first button to the last.
  const shiftTabs: [string, KeyboardEventInit & { keyCode?: number }][] = [
    ["key Tab", { key: "Tab", code: "Tab", shiftKey: true }],
    ["code Tab only", { key: "Unidentified", code: "Tab", shiftKey: true }],
    ["keyCode 9 only", { key: "Unidentified", code: "", keyCode: 9, shiftKey: true }],
    ["key ISO_Left_Tab", { key: "ISO_Left_Tab", code: "", shiftKey: true }],
  ];
  for (const [name, init] of shiftTabs) {
    it(`Shift+Tab (${name}) from the first button goes to the last, and back again`, () => {
      const { keep, shorten } = setup();
      expect(document.activeElement).toBe(keep);
      expect(key(keep, init).defaultPrevented).toBe(true);
      expect(document.activeElement).toBe(shorten);
      key(shorten, init);
      expect(document.activeElement).toBe(keep);
      key(keep, { ...init, shiftKey: false });
      expect(document.activeElement).toBe(shorten);
    });
  }

  it("Tab and Shift+Tab pressed with focus nowhere put focus on a button", () => {
    const { keep, shorten } = setup();
    keep.blur();
    expect(key(document.body, { key: "Tab", shiftKey: true }).defaultPrevented).toBe(true);
    expect([keep, shorten]).toContain(document.activeElement);
  });

  it("the dialog box itself is never a Tab stop, and focus put on it goes to a button", () => {
    const { m, keep, shorten } = setup();
    expect(m.el.getAttribute("tabindex")).toBe("-1");
    m.el.focus(); // as Shift+Tab from Keep did in the AppImage
    expect(document.activeElement).toBe(shorten);
    m.el.focus();
    expect(document.activeElement).toBe(keep);
  });

  it("focus that leaves for nowhere comes back to the button it left", async () => {
    const { keep, shorten } = setup();
    shorten.focus();
    shorten.blur();
    expect(document.activeElement).toBe(document.body);
    await tick();
    expect(document.activeElement).toBe(shorten);
    void keep;
  });

  it("focus moved behind the question comes back to it", () => {
    const { m, other } = setup();
    other.focus();
    expect(m.el.contains(document.activeElement)).toBe(true);
  });
});

describe("the page behind the modal question stays still (click-test N1)", () => {
  const SCROLL = ["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight", "PageDown", "PageUp", "Home", "End", " "];

  it("scroll keys do nothing with focus on a button or nowhere; Space and Enter still press a button", () => {
    const { keep } = setup();
    for (const k of SCROLL.filter((k) => k !== " ")) expect(key(keep, { key: k }).defaultPrevented, `${k} on Keep`).toBe(true);
    for (const k of SCROLL) expect(key(document.body, { key: k }).defaultPrevented, `${k} with focus nowhere`).toBe(true);
    expect(key(keep, { key: " " }).defaultPrevented).toBe(false);
    expect(key(keep, { key: "Enter" }).defaultPrevented).toBe(false);
  });

  it("the wheel over the page behind does nothing", () => {
    const { behind, keep } = setup();
    for (const target of [behind, document.body, keep]) {
      const e = new WheelEvent("wheel", { bubbles: true, cancelable: true, deltaY: 400 });
      target.dispatchEvent(e);
      expect(e.defaultPrevented, target.nodeName).toBe(true);
    }
  });

  it("the page's scroll is locked while the question is open, and only then", () => {
    document.documentElement.style.overflow = "";
    document.body.style.overflow = "auto";
    const { m } = setup();
    expect(document.documentElement.style.overflow).toBe("hidden");
    expect(document.body.style.overflow).toBe("hidden");
    m.close();
    expect(document.documentElement.style.overflow).toBe("");
    expect(document.body.style.overflow).toBe("auto");
    const e = new WheelEvent("wheel", { bubbles: true, cancelable: true, deltaY: 400 });
    document.body.dispatchEvent(e);
    expect(e.defaultPrevented).toBe(false);
    expect(key(document.body, { key: "ArrowDown" }).defaultPrevented).toBe(false);
  });
});

describe("the modal question is the platform's modal, and inert behind it", () => {
  it("opens with the platform's showModal() where the webview has it", () => {
    const calls: HTMLDialogElement[] = [];
    const proto = HTMLDialogElement.prototype as { showModal?: () => void };
    const had = Object.prototype.hasOwnProperty.call(proto, "showModal") ? proto.showModal : undefined;
    proto.showModal = function (this: HTMLDialogElement) {
      calls.push(this);
      this.setAttribute("open", "");
    };
    try {
      const { m } = setup();
      expect(calls).toEqual([m.el]);
    } finally {
      if (had) proto.showModal = had;
      else delete proto.showModal;
    }
  });

  it("makes everything else on the page inert while open, and lifts exactly that on close", () => {
    const already = document.createElement("div");
    already.setAttribute("inert", "");
    const { m, behind } = setup([already]);
    expect(behind.hasAttribute("inert")).toBe(true);
    expect(m.el.closest("[inert]")).toBeNull();
    m.close();
    expect(behind.hasAttribute("inert")).toBe(false);
    expect(already.hasAttribute("inert")).toBe(true);
  });

  it("is an alert dialog with its name and description, and Escape answers it", () => {
    const { m, answers } = setup();
    expect(m.el.getAttribute("role")).toBe("alertdialog");
    expect(m.el.getAttribute("aria-modal")).toBe("true");
    expect(document.getElementById(m.el.getAttribute("aria-labelledby")!)?.textContent).toBe("Shorten the active window?");
    expect(document.getElementById(m.el.getAttribute("aria-describedby")!)?.textContent).toContain("from 30 to 7 days");
    key(document.body, { key: "Escape" });
    expect(answers).toEqual(["cancel"]);
  });
});
