// The treemap (spec AC-4, NEG-11): the largest children of a folder as squarified tiles.
// Each tile is a button labelled with its name, size and tag (colour is never the only signal);
// a folder tile drills down; the breadcrumb goes back up.

import { hierarchy, treemap, treemapSquarify } from "d3-hierarchy";
import type { Backend, Tile, Tiles } from "../api";
import { errorMessage } from "../api";
import { h, replace } from "../dom";
import { formatBytes, formatPercent, percentOf } from "../format";

export const MAX_TILES = 60;

export interface Placed {
  tile: Tile;
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Lay tiles out in a width x height box. Zero-byte tiles are left out; nothing to show is []. */
export function layout(tiles: Tile[], width: number, height: number): Placed[] {
  const shown = tiles.filter((t) => t.alloc > 0);
  if (shown.length === 0 || width <= 0 || height <= 0) return [];
  const root = hierarchy<{ tile?: Tile; children?: { tile: Tile }[] }>({
    children: shown.map((tile) => ({ tile })),
  }).sum((d) => d.tile?.alloc ?? 0);
  treemap<{ tile?: Tile; children?: { tile: Tile }[] }>().tile(treemapSquarify).size([width, height]).padding(1)(root);
  return root.leaves().flatMap((l) => {
    const n = l as unknown as { x0: number; y0: number; x1: number; y1: number };
    const tile = l.data.tile;
    return tile ? [{ tile, x: n.x0, y: n.y0, w: n.x1 - n.x0, h: n.y1 - n.y0 }] : [];
  });
}

export class Treemap {
  readonly el: HTMLElement;
  private crumbs: HTMLElement;
  private box: HTMLDivElement;
  private node: number | null = null;

  constructor(
    private backend: Backend,
    private view: number,
    private status: (text: string) => void,
  ) {
    this.crumbs = h("nav", { "aria-label": "Treemap path", class: "crumbs" });
    this.box = h("div", { class: "tm-box", role: "group", "aria-label": "Treemap tiles" });
    this.el = h("section", { class: "treemap", "aria-label": "Treemap" }, this.crumbs, this.box);
  }

  async load(node: number | null = this.node): Promise<void> {
    try {
      const t = await this.backend.treeTiles(this.view, node, MAX_TILES);
      this.node = node;
      this.render(t);
    } catch (e) {
      this.status(errorMessage(e));
    }
  }

  private render(t: Tiles): void {
    replace(
      this.crumbs,
      h("button", { type: "button", class: "link", onclick: () => void this.load(null) }, "All scanned folders"),
      ...t.breadcrumb.flatMap((c) => [" / ", h("button", { type: "button", class: "link", onclick: () => void this.load(c.id) }, c.name)]),
    );
    const width = this.box.clientWidth || 800;
    const height = this.box.clientHeight || 480;
    const placed = layout(t.tiles, width, height);
    if (placed.length === 0) {
      replace(this.box, h("p", { class: "empty" }, "Nothing to show here: no allocated bytes."));
      return;
    }
    replace(
      this.box,
      ...placed.map((p) => {
        const share = formatPercent(percentOf(p.tile.alloc, t.total_alloc));
        const what = p.tile.id === null ? "everything else" : p.tile.is_dir ? "folder" : "file";
        const tag = p.tile.tag ? `, ${p.tile.tag}` : "";
        // The visible text (name, then size and tag) comes first and in the same order, so a
        // speech user can name the tile by what it shows (axe label-content-name-mismatch).
        const label = `${p.tile.name}: ${formatBytes(p.tile.alloc)}${tag}, ${share}, ${what}`;
        const drill = p.tile.is_dir && p.tile.id !== null;
        return h(
          "button",
          {
            type: "button",
            class: `tile${p.tile.tag ? ` tag-${p.tile.tag}` : ""}${p.tile.is_dir ? " dir" : ""}`,
            "aria-label": drill ? `${label}. Open` : label,
            title: label,
            "aria-disabled": drill ? null : "true",
            style: `left:${p.x}px;top:${p.y}px;width:${p.w}px;height:${p.h}px`,
            onclick: () => {
              if (drill) void this.load(p.tile.id);
            },
          },
          p.w > 60 && p.h > 24 ? h("span", { class: "tile-name" }, p.tile.name) : null,
          // A space between the two lines, so their words do not run together.
          p.w > 60 && p.h > 40 ? " " : null,
          p.w > 60 && p.h > 40 ? h("span", { class: "tile-size" }, `${formatBytes(p.tile.alloc)}${tag}`) : null,
        );
      }),
    );
  }
}
