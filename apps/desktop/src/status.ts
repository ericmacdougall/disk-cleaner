// The one owner of the app's status line (click test CT-F6). Nothing else writes the line.
//
// Scopes are paths: "chooser", "scan", "failed", "results/<view>" (the results bar: export),
// "results/<view>/<tab>" (one tab's view). The app enters the scope of the screen it shows.
// - A message belongs to the scope that wrote it. Entering a scope outside it clears it, so a
//   message never shows on a screen it does not describe. A results-bar message stays while the
//   user changes tab in the same results; a tab's message goes with its tab.
// - An operation (`begin`) is one user action with a late answer: export, open, add folders.
//   Starting one clears the line and supersedes every earlier operation; leaving its scope ends
//   it. A superseded or ended operation's late `set` writes nothing.
// - A view's setter (`setter`) is bound to its scope by name: views are kept across tab changes,
//   so a view writes again when its tab is shown again, and never while another screen shows.

import { h } from "./dom";

export interface Operation {
  /** Show `text` for this operation, if it is still the latest one and its screen still shows. */
  set(text: string): void;
  /** Still the latest operation, on its screen. */
  readonly live: boolean;
}

/** `scope` is `outer` or inside it. */
export function within(scope: string, outer: string): boolean {
  return scope === outer || scope.startsWith(`${outer}/`);
}

export class StatusLine {
  readonly el: HTMLElement;
  private where = "";
  private owner = "";
  private latest: { scope: string } | null = null;

  constructor() {
    this.el = h("p", { class: "live", role: "status", "aria-live": "polite" });
  }

  /** The scope of the screen shown. */
  get scope(): string {
    return this.where;
  }

  /** The text shown. */
  get text(): string {
    return this.el.textContent ?? "";
  }

  /** Show the screen of `scope`. Re-entering the scope shown changes nothing. */
  enter(scope: string): void {
    if (scope === this.where) return;
    this.where = scope;
    if (this.text !== "" && !within(scope, this.owner)) this.show("", scope);
    if (this.latest && !within(scope, this.latest.scope)) this.latest = null;
  }

  /** Start an operation on `scope` (default: the screen shown). */
  begin(scope: string = this.where): Operation {
    const op = { scope };
    this.latest = op;
    this.show("", scope);
    const alive = () => this.latest === op && within(this.where, op.scope);
    return {
      set: (text) => {
        if (alive()) this.show(text, op.scope);
      },
      get live() {
        return alive();
      },
    };
  }

  /** A writer for a view on `scope`: it writes only while that scope (or one inside it) shows. */
  setter(scope: string): (text: string) => void {
    return (text) => {
      if (within(this.where, scope)) this.show(text, scope);
    };
  }

  private show(text: string, scope: string): void {
    this.owner = scope;
    this.el.textContent = text;
  }
}
