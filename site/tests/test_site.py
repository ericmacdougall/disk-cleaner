"""Checks for the static site in site/. Standard library only.

Run from the repository root:
    python3 -m unittest discover -s site/tests -v
"""
import re
import unittest
import xml.etree.ElementTree as ET
from html.parser import HTMLParser
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SITE = ROOT / "site"
INDEX = SITE / "index.html"
BRAND_MD = SITE / "brand" / "BRAND.md"
README = SITE / "README.md"
PAGES_YML = ROOT / ".github" / "workflows" / "pages.yml"
SITE_YML = ROOT / ".github" / "workflows" / "site.yml"
ALLOWED_LINK_PREFIX = "https://github.com/ericmacdougall/"
VOID = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"}
UNIT_CMD = "python3 -m unittest discover -s site/tests -v"


def read(path):
    return Path(path).read_text(encoding="utf-8")


class Collector(HTMLParser):
    """Collects structure from the page in one pass."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.stack, self.errors = [], []
        self.ids, self.tags, self.headings = [], [], []
        self.metas, self.links, self.anchors = [], [], []
        self.html_attrs = {}
        self.title, self._in_title = "", False
        self.svgs = []  # dicts: attrs, has_title
        self.imgs = []
        self._svg_stack = []

    def _start(self, tag, attrs):
        d = dict(attrs)
        self.tags.append((tag, d))
        if "id" in d:
            self.ids.append(d["id"])
        if tag == "html":
            self.html_attrs = d
        elif tag == "meta":
            self.metas.append(d)
        elif tag == "link":
            self.links.append(d)
        elif tag == "img":
            self.imgs.append(d)
        elif tag == "a" and d.get("href", "").startswith("#"):
            self.anchors.append(d["href"][1:])
        elif tag in ("h1", "h2", "h3", "h4", "h5", "h6"):
            self.headings.append(int(tag[1]))
        elif tag == "svg":
            self.svgs.append({"attrs": d, "has_title": False})
            self._svg_stack.append(self.svgs[-1])
        elif tag == "title":
            if self._svg_stack:
                self._svg_stack[-1]["has_title"] = True
            else:
                self._in_title = True

    def handle_starttag(self, tag, attrs):
        self._start(tag, attrs)
        if tag not in VOID:
            self.stack.append(tag)

    def handle_startendtag(self, tag, attrs):
        self._start(tag, attrs)
        if tag == "svg":
            self._svg_stack.pop()

    def handle_endtag(self, tag):
        if tag in VOID:
            return
        if self.stack and self.stack[-1] == tag:
            self.stack.pop()
            if tag == "svg" and self._svg_stack:
                self._svg_stack.pop()
            if tag == "title":
                self._in_title = False
        else:
            self.errors.append((tag, list(self.stack[-3:]), self.getpos()))

    def handle_data(self, data):
        if self._in_title:
            self.title += data


def parse_page():
    c = Collector()
    c.feed(read(INDEX))
    c.close()
    return c


def external_refs(markup):
    """References that would make the browser fetch something, or links off the allowed prefix."""
    found = []
    for m in re.finditer(r"""\b(?:src|srcset|poster|data)\s*=\s*["']?\s*(?:https?:)?//[^"'\s>]+""", markup, re.I):
        found.append(m.group(0))
    for m in re.finditer(r"""<link\b[^>]*\bhref\s*=\s*["']?\s*(?:https?:)?//[^"'\s>]+""", markup, re.I):
        found.append(m.group(0))
    for m in re.finditer(r"""@import\b[^;]*""", markup, re.I):
        found.append(m.group(0))
    for m in re.finditer(r"""url\(\s*["']?\s*(?:https?:)?//[^)]*\)""", markup, re.I):
        found.append(m.group(0))
    for m in re.finditer(r"""<a\b[^>]*\bhref\s*=\s*["'](https?://[^"']+)["']""", markup, re.I):
        if not m.group(1).startswith(ALLOWED_LINK_PREFIX):
            found.append(m.group(0))
    return found


def forbidden_hits(text, words):
    low = text.lower()
    return [w for w in words if w.lower() in low]


def top_level_permissions(yml):
    perms, inside = {}, False
    for line in yml.splitlines():
        if re.match(r"^permissions:\s*$", line):
            inside = True
            continue
        if inside:
            m = re.match(r"^\s+([a-z-]+):\s*(\S+)\s*$", line)
            if m:
                perms[m.group(1)] = m.group(2)
            elif line.strip() and not line.startswith(" "):
                break
    return perms


def action_refs(yml):
    return re.findall(r"^\s*-?\s*uses:\s*(\S+)", yml, re.M)


def css_tokens(html):
    """(light, dark) dicts of custom properties from the inline stylesheet."""
    style = re.search(r"<style>(.*?)</style>", html, re.S).group(1)
    root = re.search(r":root\s*\{(.*?)\}", style, re.S).group(1)
    dark_block = re.search(r"@media \(prefers-color-scheme: dark\)\s*\{\s*:root\s*\{(.*?)\}", style, re.S).group(1)
    pat = re.compile(r"(--[a-z-]+):\s*(#[0-9a-fA-F]{6})\s*;")
    return dict(pat.findall(root)), dict(pat.findall(dark_block))


class TestDetectorsAreNotVacuous(unittest.TestCase):
    """The detectors below must fire on known-bad input, so the real tests cannot pass by being blind."""

    def test_external_detector_fires(self):
        bad = [
            '<img src="https://cdn.example.com/a.png">',
            '<link rel="stylesheet" href="https://fonts.example.com/css">',
            "<style>@import url(https://x.example/a.css);</style>",
            "<style>a{background:url(//x.example/a.png)}</style>",
            '<a href="https://example.com/">x</a>',
        ]
        for snippet in bad:
            self.assertTrue(external_refs(snippet), snippet)
        self.assertFalse(external_refs('<a href="https://github.com/ericmacdougall/disk-cleaner">ok</a>'))
        self.assertFalse(external_refs('<img src="brand/logo.svg">'))

    def test_forbidden_detector_fires(self):
        self.assertEqual(forbidden_hits("Get it at PruneDisk.com now, the FASTEST", ["prunedisk.com", "fastest"]),
                         ["prunedisk.com", "fastest"])
        self.assertEqual(forbidden_hits("nothing here", ["fastest"]), [])

    def test_structure_detector_fires(self):
        c = Collector()
        c.feed('<div id="a"><p id="a"></div>')
        self.assertTrue(c.errors or c.stack)
        self.assertEqual(c.ids, ["a", "a"])

    def test_permissions_and_pin_parsers_fire(self):
        yml = "permissions:\n  contents: read\n  issues: write\njobs:\n  a:\n    steps:\n      - uses: actions/x@v4\n"
        self.assertEqual(top_level_permissions(yml), {"contents": "read", "issues": "write"})
        self.assertFalse(re.fullmatch(r".+@[0-9a-f]{40}", action_refs(yml)[0]))


class TestPageStructure(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.page = parse_page()
        cls.html = read(INDEX)

    def test_tags_balance(self):
        self.assertEqual(self.page.errors, [], "mismatched closing tags")
        self.assertEqual(self.page.stack, [], "unclosed tags")

    def test_no_duplicate_ids(self):
        dupes = sorted({i for i in self.page.ids if self.page.ids.count(i) > 1})
        self.assertEqual(dupes, [])

    def test_exactly_one_h1(self):
        self.assertEqual(self.page.headings.count(1), 1)

    def test_no_skipped_heading_levels(self):
        prev = 0
        for h in self.page.headings:
            self.assertLessEqual(h, prev + 1, f"h{h} follows h{prev}")
            prev = h

    def test_html_lang(self):
        self.assertTrue(self.page.html_attrs.get("lang"), "missing lang on <html>")

    def test_viewport_meta(self):
        v = [m for m in self.page.metas if m.get("name") == "viewport"]
        self.assertEqual(len(v), 1)
        self.assertIn("width=device-width", v[0].get("content", ""))

    def test_title(self):
        self.assertTrue(self.page.title.strip())
        self.assertIn("PruneDisk", self.page.title)

    def test_meta_description(self):
        d = [m for m in self.page.metas if m.get("name") == "description"]
        self.assertEqual(len(d), 1)
        text = d[0].get("content", "")
        self.assertTrue(text)
        self.assertLessEqual(len(text), 155, len(text))
        self.assertTrue(text.startswith("Early"), "description must lead with the status")

    def test_open_graph_tags(self):
        props = {m.get("property"): m.get("content", "") for m in self.page.metas if m.get("property")}
        for key in ("og:type", "og:title", "og:description", "og:site_name"):
            self.assertTrue(props.get(key), f"missing {key}")
        self.assertTrue(props["og:description"].startswith("Early"))
        self.assertNotIn("og:url", props, "no og:url until a real address exists")
        self.assertNotIn("og:image", props, "no og:image until a real image exists")

    def test_landmarks(self):
        names = [t for t, _ in self.page.tags]
        for t in ("header", "nav", "main", "footer"):
            self.assertEqual(names.count(t), 1, t)

    def test_in_page_anchors_resolve(self):
        missing = [a for a in self.page.anchors if a not in self.page.ids]
        self.assertEqual(missing, [])
        self.assertTrue(self.page.anchors, "expected in-page anchors")

    def test_images_and_svgs_have_text_alternatives(self):
        for img in self.page.imgs:
            self.assertIn("alt", img, f"img without alt: {img}")
        for svg in self.page.svgs:
            a = svg["attrs"]
            ok = (a.get("aria-hidden") == "true" or a.get("aria-label") or a.get("aria-labelledby") or svg["has_title"])
            self.assertTrue(ok, f"svg without a text alternative: {a}")
        self.assertTrue(self.page.svgs)

    def test_pre_has_role_and_label(self):
        pres = [d for t, d in self.page.tags if t == "pre"]
        self.assertTrue(pres)
        for d in pres:
            self.assertEqual(d.get("role"), "region")
            self.assertTrue(d.get("aria-label"))
            self.assertIn("tabindex", d)

    def test_reduced_motion_and_dark_scheme_present(self):
        self.assertIn("prefers-reduced-motion", self.html)
        self.assertIn("prefers-color-scheme: dark", self.html)


class TestNoExternalResources(unittest.TestCase):
    def test_no_script_elements(self):
        c = parse_page()
        self.assertEqual([t for t, _ in c.tags if t == "script"], [])

    def test_no_external_resources_in_page(self):
        self.assertEqual(external_refs(read(INDEX)), [])

    def test_no_external_resources_in_svgs(self):
        for f in sorted((SITE / "brand").glob("*.svg")):
            self.assertEqual(external_refs(read(f)), [], f.name)


class TestContent(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.html = read(INDEX)
        cls.text = re.sub(r"<[^>]+>", " ", cls.html)

    def test_forbidden_strings_absent(self):
        bad = ["prunedisk.com", "prunedisk.app", "fastest", "download now", "clean your disk"]
        self.assertEqual(forbidden_hits(self.html, bad), [])

    def test_british_spellings_absent(self):
        bad = ["colour", "licence", "judgement", "analyser", "organisation", "behaviour", "favour", "centre"]
        for f in (INDEX, README, BRAND_MD):
            self.assertEqual(forbidden_hits(read(f), bad), [], f.name)

    def test_status_wording_present(self):
        self.assertIn("Early.", self.text)
        self.assertIn("It deletes nothing", self.text)
        self.assertRegex(self.text, r"read-only")
        self.assertIn("in review", self.text.lower())

    def test_no_download_button_or_install_claim(self):
        self.assertNotRegex(self.html, r'(?i)<a[^>]*class="btn[^"]*"[^>]*>[^<]*download')
        self.assertIn("nothing to install yet", re.sub(r"\s+", " ", self.text))

    def test_repo_and_command_still_disk_cleaner(self):
        flat = re.sub(r"\s+", " ", self.text)
        self.assertIn("repository is still named disk-cleaner", flat)
        self.assertIn("command is still called", flat)
        self.assertIn("$ disk-cleaner scan", self.html)
        self.assertIn("https://github.com/ericmacdougall/disk-cleaner", self.html)

    def test_platforms_are_stated_as_targets(self):
        flat = re.sub(r"\s+", " ", self.text)
        self.assertIn("target platforms", flat)
        self.assertIn("only macOS has been tested by hand", flat)

    def test_page_weight_under_300_kb(self):
        total = INDEX.stat().st_size + sum(f.stat().st_size for f in (SITE / "brand").glob("*.svg"))
        self.assertLess(total, 300 * 1024, total)


class TestBrand(unittest.TestCase):
    def test_svgs_parse_and_have_viewbox(self):
        svgs = sorted((SITE / "brand").glob("*.svg"))
        self.assertGreaterEqual(len(svgs), 5)
        for f in svgs:
            root = ET.fromstring(read(f))
            self.assertTrue(root.tag.endswith("svg"), f.name)
            self.assertTrue(root.get("viewBox"), f"{f.name} lacks viewBox")

    def test_standalone_logos_do_not_switch_by_scheme(self):
        for name in ("logo.svg", "logo-dark.svg", "logo-mark.svg", "logo-mark-dark.svg"):
            self.assertNotIn("prefers-color-scheme", read(SITE / "brand" / name), name)

    def test_brand_tokens_match_css(self):
        light, dark = css_tokens(read(INDEX))
        rows = {}
        for m in re.finditer(r"^\|\s*`(--[a-z-]+)`\s*\|\s*`(#[0-9a-fA-F]{6})`\s*\|\s*`(#[0-9a-fA-F]{6})`", read(BRAND_MD), re.M):
            rows[m.group(1)] = (m.group(2).lower(), m.group(3).lower())
        for tok in ("--bg", "--surface", "--text", "--muted", "--accent", "--border", "--rule"):
            self.assertIn(tok, rows, f"{tok} missing from BRAND.md")
            self.assertEqual(rows[tok], (light[tok].lower(), dark[tok].lower()), tok)

    def test_class_colors_in_brand_doc_match_css(self):
        light, dark = css_tokens(read(INDEX))
        doc = read(BRAND_MD).lower()
        for tok in [k for k in light if k.startswith("--c-")]:
            self.assertIn(light[tok].lower(), doc, f"light {tok}")
            self.assertIn(dark[tok].lower(), doc, f"dark {tok}")

    def test_logo_colors_use_tokens(self):
        light, dark = css_tokens(read(INDEX))
        logo = read(SITE / "brand" / "logo.svg").lower()
        logo_dark = read(SITE / "brand" / "logo-dark.svg").lower()
        for tok in ("--accent", "--c-regen", "--c-repo"):
            self.assertIn(light[tok].lower(), logo)
            self.assertIn(dark[tok].lower(), logo_dark)


class TestWorkflows(unittest.TestCase):
    def check_pinned(self, path):
        refs = action_refs(read(path))
        self.assertTrue(refs, f"{path.name} uses no actions?")
        for r in refs:
            self.assertRegex(r, r"^[\w./-]+@[0-9a-f]{40}$", f"{path.name}: {r} is not pinned to a 40-character SHA")

    def test_pages_actions_pinned(self):
        self.check_pinned(PAGES_YML)

    def test_site_ci_actions_pinned(self):
        self.check_pinned(SITE_YML)

    def test_pages_permissions_exact(self):
        self.assertEqual(top_level_permissions(read(PAGES_YML)),
                         {"contents": "read", "pages": "write", "id-token": "write"})

    def test_site_ci_permissions_read_only(self):
        self.assertEqual(top_level_permissions(read(SITE_YML)), {"contents": "read"})

    def test_site_ci_triggers_and_command(self):
        y = read(SITE_YML)
        for needle in ("push:", "pull_request:", '"site/**"', ".github/workflows/site.yml",
                       ".github/workflows/pages.yml", "ubuntu-latest", UNIT_CMD):
            self.assertIn(needle, y, needle)

    def test_pages_runs_tests_before_upload(self):
        y = read(PAGES_YML)
        self.assertIn(UNIT_CMD, y)
        self.assertLess(y.index(UNIT_CMD), y.index("upload-pages-artifact"))

    def test_pages_removes_non_public_files_before_upload(self):
        y = read(PAGES_YML)
        rm = [l for l in y.splitlines() if re.search(r"\brm\b", l)]
        self.assertEqual(len(rm), 1, rm)
        for p in ("site/README.md", "site/brand/BRAND.md", "site/tests"):
            self.assertIn(p, rm[0])
        self.assertLess(y.index(rm[0]), y.index("upload-pages-artifact"))

    def test_pages_does_not_deploy_on_pull_requests(self):
        y = read(PAGES_YML)
        self.assertNotIn("pull_request", y)
        self.assertIn("branches: [main]", y)


if __name__ == "__main__":
    unittest.main()
