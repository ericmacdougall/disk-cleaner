# disk-cleaner brand

Status: proposal for the project owner. Nothing here renames the project.

## 1. Name

- **Repository, crate and command: `disk-cleaner`.** Fixed. Always lower case, always with the hyphen,
  always in code font or the wordmark when it names the tool or the command.
- **Display name (recommendation):** keep `disk-cleaner` as the display name too, set in the wordmark
  below. In running prose write "disk-cleaner" (lower case, even at the start of a sentence, the way
  `git` and `curl` are written). This avoids inventing a second name before the name question in
  section 7 is decided.
- **Wordmark:** the three-block mark followed by `disk-cleaner` in the system monospace stack, weight 600.
  Minimum clear space: the height of one small block on every side. Minimum size: 16 px for the mark alone
  (`favicon.svg`), 120 px wide for the full logo.
- **Do not** write "Disk Cleaner" in title case, "DiskCleaner", or use the name next to the logo of
  another product.

## 2. Tagline

Recommended (one): **Know what is safe to remove.**
Why: it says what the tool does that others do not (classify before removing), makes no size or speed
claim, and stays true while only the read-only scanner exists.

Alternates:

1. Disk space for machines full of repos and agents.
2. Remove nothing that exists only here.

Do not use: "fastest", "one click", "clean your disk", "reclaim gigabytes", or any number that has not
been measured and published with its method.

## 3. Voice

Plain, specific, calm. Say what a thing is, then what it does. Prefer the concrete noun
(`node_modules`, "an unpushed branch") to the abstract one ("junk", "bloat"). Say "early" and "planned"
where they are true; never write a future feature in the present tense. State limits next to claims.
No exclamation marks, no emoji, no fear ("your disk is dying"), no superlatives. Examples invented for
the site use obviously synthetic paths such as `~/projects/example-app`.

| Instead of | Write |
|---|---|
| Supercharge your dev machine | Shows what fills a developer's disk and what is safe to remove. |
| Cleans your disk in one click | Plans removals you can read first. In the current version nothing is removed. |
| Safe and secure | Unknown means keep. A check that fails counts as "keep". |

## 4. Colour tokens

Text pairs are WCAG 2.2 AA at normal size (4.5:1) or better. Non-text parts (borders that mark a
control, chart tiles) are 3:1 or better. Ratios were computed with the WCAG relative-luminance formula
from the hex values below.

| Token | Light | Dark | Use |
|---|---|---|---|
| `--bg` | `#fbfaf6` | `#0f1513` | page |
| `--surface` | `#f1eee6` | `#18201d` | cards, code |
| `--text` | `#1b2420` | `#e8eeea` | body text |
| `--muted` | `#485650` | `#a8b6af` | secondary text |
| `--accent` | `#0b5d56` | `#5ed6c8` | links, buttons, logo main block |
| `--border` | `#78837d` | `#6f7f78` | borders of controls and callouts |
| `--rule` | `#d9d5c9` | `#2a3531` | decorative dividers only |

Contrast, light: text on bg 15.2, text on surface 13.7, muted on bg 7.4, muted on surface 6.7,
accent on bg 7.4, accent on surface 6.7, white on accent (button) 7.7, border on bg 3.8, border on
surface 3.4.
Contrast, dark: text on bg 15.7, text on surface 14.1, muted on bg 8.8, muted on surface 7.9, accent
on bg 10.5, accent on surface 9.4, bg on accent (button) 10.5, border on bg 4.4, border on surface 3.9.

Class colours (one per file class; always paired with a text label, never the only signal):

| Class | Light | Dark | Label text on tile, light / dark |
|---|---|---|---|
| repo, worktree | `#2f5fb3`, `#6d49b8` | `#7aa7f0`, `#b79cf0` | 6.2, 6.4 / 7.6, 7.9 |
| regenerable | `#b45309` | `#f0a54a` | 5.0 / 9.0 |
| cache | `#9a3f1b` | `#e8855a` | 6.8 / 7.0 |
| agent-session | `#a3306f` | `#e879b4` | 6.5 / 6.9 |
| output | `#1f7a4d` | `#5fcf93` | 5.3 / 9.5 |
| loose-code | `#5f6b66` | `#9aa7a1` | 5.6 / 7.4 |

Label text is white on the light tiles and `#0f1513` on the dark tiles. Each class colour also
clears 4.3:1 or more against both page backgrounds in its own scheme, so it is usable as a swatch or
a thin line.

The three logo colours are `--accent`, the regenerable colour and the repo colour of the active scheme.

## 5. Type

System font stacks only. Nothing is downloaded, embedded or licensed.

- Text: `system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif`
- Code and wordmark: `ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace`

These are the operating system's own fonts, used by name. No font file is distributed with the
project, so there is no font licence to carry. If the project later bundles a typeface it must be
open-licensed (SIL OFL or Apache-2.0), self-hosted, and its licence added to the repository.

Scale: base 16 px, line height 1.6; headings clamp from 1.4 rem to 3 rem. Code never goes below 13 px.

## 6. Logo

- **Mark** (`logo-mark.svg`): a square cut into one large block and two small ones, a treemap in three
  tiles. It says what the tool shows (where space went, by kind) and the large block is the part that
  stays. Three flat tiles with generous gaps stay distinguishable at 16 px; `favicon.svg` is the same
  mark redrawn on a 32-unit grid with wider relative gaps. Colour is carried by the three tiles, so it
  works on light and dark: each SVG switches its fills with `prefers-color-scheme` internally.
- **Logo** (`logo.svg`): the mark plus the wordmark. The wordmark is live text in the system monospace
  stack, so its exact glyphs depend on the viewer's system. A version with outlined letters is not
  included, because no open font was chosen; make one only when a typeface is.
- Drawn by hand as plain rectangles. No stock icon, no generated image, no shape borrowed from another
  product. Do not rotate, outline, add shadows or recolour outside the tokens above.

## 7. Name availability (decision for the project owner)

Checked 18 and 19 November 2026 with each service's public search interface or API; counts change daily.
The Microsoft Store has no public search API, so that row is a web search and is not exhaustive.

Existing products named "Disk Cleaner" or `disk-cleaner`:

| Where | Finding |
|---|---|
| GitHub | 481 repositories with "disk-cleaner" in the name, none dominant. Most starred: [vudsen/ai-disk-cleaner](https://github.com/vudsen/ai-disk-cleaner), [gouthamkb06-ux/advanced-disk-cleaner](https://github.com/gouthamkb06-ux/advanced-disk-cleaner), [cyb3rl00m/safe-disk-cleaner](https://github.com/cyb3rl00m/safe-disk-cleaner), [ozankasikci/rust-disk-cleaner](https://github.com/ozankasikci/rust-disk-cleaner). Search: <https://github.com/search?q=disk-cleaner+in%3Aname&type=repositories> |
| crates.io | The crate name `disk-cleaner` is **taken** ("A disk usage cli similar to windirstat"): <https://crates.io/crates/disk-cleaner> |
| npm | `disk-cleaner` itself is free. Many near names exist, for example [dsh-disk-cleaner](https://www.npmjs.com/package/dsh-disk-cleaner). |
| Snap Store | A snap named `disk-cleaner` exists, plus [easy-disk-cleaner](https://snapcraft.io/easy-disk-cleaner): <https://snapcraft.io/disk-cleaner> |
| Flathub | No app with that exact name. Similar tools exist (Czkawka, Disk Usage Analyzer, Prunr): <https://flathub.org/apps/search?q=disk-cleaner> |
| Microsoft Store | No exact "Disk Cleaner" found by web search; several similarly named cleaners (Disk Clean, PowerDisk, Disko). Not verified in the Store's own search. |
| Mac App Store | [Disk Cleaner - Free HD Space](https://apps.apple.com/us/app/disk-cleaner-free-hd-space/id970246631) already exists, as do many similar cleaners. |

Reading: the generic name is crowded. The repository name is fine for a GitHub project, but the crate
name is already used by another project (the binary can still be called `disk-cleaner`; publishing the
crate would need a different crate name), and an app-store listing called "Disk Cleaner" would sit
beside an existing product with that exact title. **This is the owner's decision; nothing has been renamed.**

Three distinctive alternative display names, same checks (crates.io and npm exact name, GitHub name search, Snap, Flathub, Mac App Store search via their APIs on the same dates; Microsoft Store not checked for these):

| Name | Result |
|---|---|
| **Shedwise** | crates.io and npm exact names free. GitHub: 10 repositories with the word, none a disk tool (an unrelated GitHub organisation named `shedwise` exists). No Snap, Flathub or Mac App Store hit. Cleanest result. |
| **Slackspace** | crates.io and npm exact names free. GitHub: 24 repositories, top ones are unrelated demos ([example](https://github.com/cternes/slackspace-angular-spring-keycloak)). No Snap, Flathub or Mac App Store hit. "Slack space" is a real filesystem term, which is meaningful but may confuse a search for the messaging product. |
| **Tidyroot** | crates.io and npm exact names free. GitHub: no repositories. No Snap or Flathub hit; Mac App Store shows only unrelated "Clean" apps. Clear, but the most invented of the three. |

Rejected for being taken: Ballast (crates.io and npm taken, 731 GitHub repositories), Spillway, Skerry
and Tidemark (all taken on crates.io and npm), Diskwise (npm taken, a Mac App Store app "DiskWise: Storage
Cleaner" exists).

None of this is a trademark search. Before adopting a new name publicly, a trademark search in the
target jurisdictions is needed; this document does not provide one.
