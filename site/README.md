# PruneDisk site

A static page for PruneDisk (the repository and command are still named disk-cleaner until the planned rename). No build step, no dependencies, no external requests.

```
site/
  index.html   the page, with its CSS inline (light and dark via prefers-color-scheme)
  brand/       BRAND.md, logo.svg and logo-dark.svg, logo-mark.svg and logo-mark-dark.svg, favicon.svg
```

## Preview locally

```
cd site
python3 -m http.server 8765
```

Open <http://localhost:8765/>. Stop it with Ctrl-C.

## Tests

Standard library only, run from the repository root:

```
python3 -m unittest discover -s site/tests -v
```

They check the page structure, accessibility basics, status wording, absence of external requests, the
brand tokens against the CSS, and the Pages workflows. CI runs them on every change to `site/`, and the
Pages deploy runs them first.

## Deploy

`.github/workflows/pages.yml` publishes `site/` to GitHub Pages on every push to `main` that touches
`site/`. A maintainer must first enable Pages in the repository settings with the source set to
"GitHub Actions". The workflow does not change settings.

## Why the CSS is inline

With a separate stylesheet, the built-in browser used for checking reported a layout shift of 1.0 on
first paint; with the same CSS inline it reported 0. The page is one screen of CSS, so it is inlined.

## Rules for editing

- Say plainly what exists. Check the status table against the repository before you change it.
- No scripts, web fonts, CDNs, analytics or images from other hosts. Keep the page under 300 KB.
- Keep text contrast at WCAG AA or better; the token values are in `brand/BRAND.md`.
- Paths and numbers in examples are invented. Do not paste real scan output.
