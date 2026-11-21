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
brand tokens against the CSS, and the Pages workflows. Run them locally with `./scripts/local-ci.sh`
(which also runs fmt, clippy, the Rust tests and the license check), or on their own with
`python3 -m unittest discover -s site/tests -v`. The Pages workflow also runs them first when it is started by hand.

## Publishing and rollback

- **One-time setup:** a maintainer sets the Pages source to "GitHub Actions" in the repository
  settings. The workflows do not change settings.
- **Deploy:** `.github/workflows/pages.yml` is started by hand (`gh workflow run pages.yml` or the Actions tab). Its `gate` job asks the GitHub API, read-only, whether Pages is enabled with
  GitHub Actions as its source (`python3 -I site/tools/pages_gate.py`). If it is, the `deploy` job runs the
  tests, builds the publishable copy with `python3 site/tools/prepare.py _site` (no README, BRAND.md, tests
  or tools) and deploys it. If Pages is not enabled, `deploy` is skipped with a notice and the run stays
  green; if Pages publishes from a branch instead, it is skipped with a warning. Any other answer (a token
  or permission error, an outage) fails the run rather than being read as "not enabled".
- **After enabling Pages:** nothing deploys until the Pages workflow is run by hand from the Actions tab.
- **Rehearsal:** when `.github/workflows/site.yml` is started by hand (it is disabled in the Actions
  settings, so `gh workflow enable site.yml` first, and disable it again afterwards), the `pages-rehearsal` job runs the
  same preparation script, packages the artifact without deploying, serves it under `/disk-cleaner/` (how a
  project Pages site is served) and smoke-tests it with curl. Both workflows call the same script, and a
  test checks that. The `pages-gate` job runs the same gate and shows what the deploy would decide
  today, but only on those manual runs; it no longer runs on every change.
- **Roll back:** re-run the previous successful "Pages" workflow run from the Actions tab, or revert the
  commit and push.
- **Take the site down:** disable Pages in the repository settings.

## Why the CSS is inline

With a separate stylesheet, the built-in browser used for checking reported a layout shift of 1.0 on
first paint; with the same CSS inline it reported 0. The page is one screen of CSS, so it is inlined.

## Rules for editing

- Say plainly what exists. Check the status table against the repository before you change it.
- No scripts, web fonts, CDNs, analytics or images from other hosts. Keep the page under 300 KB.
- Keep text contrast at WCAG AA or better; the token values are in `brand/BRAND.md`.
- Paths and numbers in examples are invented. Do not paste real scan output.
