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


NUMBER_WORDS = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
                "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen",
                "eighteen", "nineteen", "twenty"]
CORE_MODEL = ROOT / "crates" / "core" / "src" / "model.rs"
CORE_PLAN = ROOT / "crates" / "core" / "src" / "plan.rs"


def core_fact_names(src=None):
    """Fact names in the order `RepoFacts::entries()` in the core crate returns them."""
    src = read(CORE_MODEL) if src is None else src
    body = re.search(r"fn entries\(&self\)[^{]*\{(.*?)\n    \}", src, re.S).group(1)
    return re.findall(r'\("([a-z_]+)",\s*self\.', body)


def core_action_names(src=None):
    """Plan action names from `Action::as_str` in the core crate."""
    src = read(CORE_PLAN) if src is None else src
    return re.findall(r'Action::\w+ => "([a-z-]+)"', src)


def example_text(html):
    return re.search(r'aria-label="Example scanner output"><code>(.*?)</code></pre>', html, re.S).group(1)


def example_fact_groups(text):
    """For each repo, worktree or bare repo in the example scan summary, its fact names in order."""
    groups, cur = [], None
    for line in text.splitlines():
        if re.match(r"^\s+\[(repo|worktree|bare-repo)\] ", line) and "=" not in line:
            cur = []
            groups.append(cur)
        elif cur is not None:
            cur.extend(re.findall(r"\b([a-z_]+)=(?:yes|NO|UNKNOWN)\b", line))
    return [g for g in groups if g]


def example_actions(text):
    """The action word of each item line after "Items:" in the example plan."""
    after = text.split("\nItems:\n", 1)
    if len(after) < 2:
        return []
    return re.findall(r"^  ([a-z][a-z-]+) +\S+ \S+ +\[", after[1], re.M)


UNITS = {"B": 1, "KiB": 1024, "MiB": 1024 ** 2, "GiB": 1024 ** 3, "TiB": 1024 ** 4}
SIZE = r"\d+(?:\.\d)? (?:B|KiB|MiB|GiB|TiB)"


def size(s):
    """A size as printed by the CLI ("1.9 GiB") as (bytes, rounding tolerance in bytes)."""
    num, unit = s.split()
    mult = UNITS[unit]
    return float(num) * mult, (0 if unit == "B" else 0.05 * mult)


def parse_example(text):
    """The parts of the example scan summary and plan that carry numbers, in printed order."""
    lines = text.splitlines()
    ex = {"classes": [], "repos": [], "regen": [], "items": [], "repo_count": None, "regen_count": None,
          "list_gaps": 0, "total": None, "scan_cmd": "", "plan_cmd": ""}
    section, cur = None, None
    for line in lines:
        if line.startswith("$ disk-cleaner scan"):
            ex["scan_cmd"] = line
        elif line.startswith("$ disk-cleaner plan"):
            ex["plan_cmd"] = line
            section = "plan"
            continue
        m = re.search(r", (" + SIZE + r") allocated", line)
        if m:
            ex["total"] = size(m.group(1))
        if line.startswith("Space by class"):
            section = "classes"
            continue
        m = re.match(r"Repos and worktrees \((\d+)\):", line)
        if m:
            section, ex["repo_count"] = "repos", int(m.group(1))
            continue
        m = re.match(r"Regenerable and cache directories \((\d+)\):", line)
        if m:
            section, ex["regen_count"] = "regen", int(m.group(1))
            continue
        if line.startswith("Items:"):
            section = "items"
            continue
        if not line.strip():
            if section in ("classes", "repos", "regen"):
                section = None
            continue
        if section == "classes":
            m = re.match(r"^  ([a-z-]+) +(" + SIZE + r")$", line)
            if m:
                ex["classes"].append((m.group(1), size(m.group(2))))
        elif section == "repos":
            m = re.match(r"^  \[([a-z-]+)\] (\S+)  (" + SIZE + r")$", line)
            if m:
                cur = {"class": m.group(1), "path": m.group(2), "total": size(m.group(3)), "facts": {},
                       "regen": None}
                ex["repos"].append(cur)
            elif line.strip() == "..." and line.startswith("  ") and not line.startswith("   "):
                ex["list_gaps"] += 1
            elif cur is not None:
                cur["facts"].update(re.findall(r"\b([a-z_]+)=(yes|NO|UNKNOWN)\b", line))
                m = re.search(r"regenerable (" + SIZE + r")", line)
                if m:
                    cur["regen"] = size(m.group(1))
        elif section == "regen":
            m = re.match(r"^ +(" + SIZE + r")  (\S+)$", line)
            if m:
                ex["regen"].append((m.group(2), size(m.group(1))))
            elif line.strip() == "...":
                ex["list_gaps"] += 1
        elif section == "items":
            m = re.match(r"^  ([a-z][a-z-]+) +(" + SIZE + r")  \[([a-z-]+)\] (\S+)$", line)
            if m:
                ex["items"].append({"action": m.group(1), "size": size(m.group(2)), "class": m.group(3),
                                    "path": m.group(4)})
    return ex


def close(a, b, extra_tol=0.0):
    """Two (bytes, tolerance) sizes agree within their combined rounding."""
    return abs(a[0] - b[0]) <= a[1] + b[1] + extra_tol


def total(sizes):
    return (sum(s[0] for s in sizes), sum(s[1] for s in sizes))


def descending(values):
    return all(values[i] >= values[i + 1] for i in range(len(values) - 1))


def example_problems(ex):
    """Every way the example's numbers, order or commands disagree with what the CLI would print."""
    p = []
    if not (ex["classes"] and ex["repos"] and ex["regen"] and ex["items"] and ex["total"]):
        return ["example is missing a section"]
    if not descending([s[0] for _, s in ex["classes"]]):
        p.append("Space by class is not sorted by size, descending")
    if not descending([r["total"][0] for r in ex["repos"]]):
        p.append("repos and worktrees are not sorted by size, descending")
    if not descending([s[0] for _, s in ex["regen"]]):
        p.append("regenerable and cache rows are not sorted by size, descending")
    if not descending([i["size"][0] for i in ex["items"]]):
        p.append("plan items are not sorted by reclaimable size, descending")
    if ex["list_gaps"]:
        p.append("a list is cut short with '...', so its rows cannot be checked against the totals")
    if ex["repo_count"] != len(ex["repos"]):
        p.append(f"Repos and worktrees ({ex['repo_count']}) lists {len(ex['repos'])}")
    if ex["regen_count"] != len(ex["regen"]):
        p.append(f"Regenerable and cache directories ({ex['regen_count']}) lists {len(ex['regen'])}")
    classes = dict(ex["classes"])
    if not close(total(classes.values()), ex["total"]):
        p.append("class totals do not add up to the allocated total")
    regen_classes = [classes[c] for c in ("regenerable", "cache") if c in classes]
    if not close(total(regen_classes), total([s for _, s in ex["regen"]])):
        p.append("regenerable + cache class totals differ from the listed directories")
    for cls in ("repo", "worktree", "bare-repo"):
        rows = [r for r in ex["repos"] if r["class"] == cls]
        if not rows and cls not in classes:
            continue
        if any(r["regen"] is None for r in rows):
            p.append(f"a {cls} row has no sizes line")
            continue
        own = (sum(r["total"][0] - r["regen"][0] for r in rows), sum(r["total"][1] + r["regen"][1] for r in rows))
        if not close(own, classes.get(cls, (0, 0))):
            p.append(f"{cls} class total differs from its rows minus their regenerable directories")
    for r in ex["repos"]:
        inside = [s for path, s in ex["regen"] if path.startswith(r["path"] + "/")]
        if r["regen"] is not None and not close(total(inside), r["regen"]):
            p.append(f"{r['path']}: regenerable size differs from the listed directories inside it")
    sizes = {path: s for path, s in ex["regen"]}
    sizes.update({r["path"]: r["total"] for r in ex["repos"]})
    facts = {r["path"]: r["facts"] for r in ex["repos"]}
    for it in ex["items"]:
        if it["path"] not in sizes:
            p.append(f"plan item {it['path']} is not in the scan")
        elif it["action"] != "keep" and it["action"] != "ask" and not close(it["size"], sizes[it["path"]]):
            p.append(f"plan item {it['path']} reclaims a different size than the scan shows")
        f = facts.get(it["path"])
        if f:
            vals = set(f.values())
            want = "ask" if "UNKNOWN" in vals else ("recover-backup-then-remove" if "NO" in vals else "backup-then-remove")
            if it["action"] not in (want, "keep", "ask"):
                p.append(f"plan item {it['path']} is {it['action']}, its facts give {want}")
    m = re.search(r"--out (\S+)", ex["scan_cmd"])
    n = re.search(r"--scan (\S+)", ex["plan_cmd"])
    if not m or not n or m.group(1) != n.group(1):
        p.append("the plan reads a scan file the example scan did not write with --out")
    return p


def class_table_defaults(html):
    """Class name to the text of its Default cell in the class table."""
    out = {}
    for m in re.finditer(r'<th scope="row" role="rowheader"><span class="dot"[^>]*></span>([^<]+)</th>'
                         r'<td[^>]*>.*?</td><td[^>]*data-label="[^"]*"[^>]*>(.*?)</td>', html):
        out[m.group(1).strip()] = re.sub(r"<[^>]+>", "", m.group(2))
    return out


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

    def test_core_and_example_parsers_fire(self):
        model = ('    pub fn entries(&self) -> [(&\'static str, Tri); 2] {\n        [\n'
                 '            ("has_network_remote", self.has_network_remote),\n'
                 '            ("stash_empty", self.stash_empty),\n        ]\n    }\n')
        self.assertEqual(core_fact_names(model), ["has_network_remote", "stash_empty"])
        self.assertEqual(core_action_names('Action::Keep => "keep",\n Action::Ask => "ask",'), ["keep", "ask"])
        self.assertGreaterEqual(len(core_fact_names()), 7, "real model.rs parsed to too few facts")
        self.assertIn("keep", core_action_names(), "real plan.rs parsed to no actions")
        old = ("Repos and worktrees (2):\n  [repo] ~/a  1.0 GiB\n      has_network_remote=yes stash_empty=NO\n"
               "      .git 1 B, regenerable 0 B, outputs 0 B\n  [worktree] ~/b  1 B\n      ...\n"
               "Items:\n  recover-then-archive    0 B     [repo] ~/a\n")
        self.assertEqual(example_fact_groups(old), [["has_network_remote", "stash_empty"]])
        self.assertEqual(example_actions(old), ["recover-then-archive"])

    def test_example_checker_fires(self):
        good = ("$ disk-cleaner scan ~/p --out s.json\nScanned ~/p\n3 files, 2 directories, 3.0 GiB allocated (3.0 GiB logical)\n\n"
                "Space by class (innermost classified ancestor):\n  regenerable     2.0 GiB\n  repo            1.0 GiB\n\n"
                "Repos and worktrees (1):\n  [repo] ~/p/a  3.0 GiB\n      has_network_remote=yes stash_empty=NO\n"
                "      .git 1.0 MiB, regenerable 2.0 GiB, outputs 0 B\n\n"
                "Regenerable and cache directories (1):\n     2.0 GiB  ~/p/a/node_modules\n\n"
                "$ disk-cleaner plan --scan s.json\nItems:\n  recover-backup-then-remove     3.0 GiB  [repo] ~/p/a\n")
        self.assertEqual(example_problems(parse_example(good)), [])
        bad = {
            "unsorted": good.replace("  regenerable     2.0 GiB\n  repo            1.0 GiB",
                                     "  repo            1.0 GiB\n  regenerable     2.0 GiB"),
            "no --out": good.replace(" --out s.json", ""),
            "wrong count": good.replace("Repos and worktrees (1)", "Repos and worktrees (3)"),
            "cut short": good.replace("     2.0 GiB  ~/p/a/node_modules\n", "     2.0 GiB  ~/p/a/node_modules\n  ...\n"),
            "class sum": good.replace("  repo            1.0 GiB", "  repo            1.9 GiB"),
            "wrong action": good.replace("stash_empty=NO", "stash_empty=UNKNOWN"),
            "size mismatch": good.replace("3.0 GiB  [repo]", "88.0 MiB  [repo]"),
        }
        for name, text in bad.items():
            self.assertTrue(example_problems(parse_example(text)), name)

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
        flat = re.sub(r"\s+", " ", self.text)
        self.assertIn("Early.", flat)
        self.assertIn("It deletes nothing", flat)
        self.assertRegex(flat, r"read-only")
        self.assertIn("merged into the main branch", flat)
        self.assertIn("not released", flat)

    def test_no_stale_review_claims(self):
        # The scanner and planner were merged into main; copy written while they were on a draft pull
        # request must not come back.
        flat = re.sub(r"\s+", " ", self.text).lower()
        stale = ["in review", "review branch", "open draft pull request, not merged",
                 "open pull request for the scanner", "still in review"]
        self.assertEqual([s for s in stale if s in flat], [])

    def test_unshipped_parts_are_planned(self):
        rows = {}
        for m in re.finditer(r'<th scope="row"[^>]*>([^<]+)</th><td[^>]*data-label="Status"[^>]*>'
                             r'<span class="badge">([^<]+)</span>', self.html):
            rows[m.group(1).strip()] = m.group(2).strip()
        for part in ("Recovery, backup, apply, restore", "Desktop app", "App stores and installers"):
            self.assertEqual(rows.get(part), "Planned", part)
        self.assertNotEqual(rows.get("CLI: scan and plan"), "Planned")
        self.assertNotIn("Released", rows.values(), "nothing is released yet")

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

    def test_fact_count_matches_core(self):
        n = len(core_fact_names())
        flat = re.sub(r"\s+", " ", self.text)
        said = re.findall(r"records (\w+) facts", flat) + re.findall(r"all (\w+) are yes", flat)
        self.assertEqual(len(said), 2, said)
        for word in said:
            self.assertEqual(NUMBER_WORDS.index(word), n, f"page says {word}, core has {n} facts")

    def test_example_lists_the_core_facts(self):
        groups = example_fact_groups(example_text(self.html))
        self.assertTrue(groups, "no repo with facts in the example")
        for g in groups:
            self.assertEqual(g, core_fact_names())

    def test_example_uses_current_action_names(self):
        actions = core_action_names()
        used = example_actions(example_text(self.html))
        self.assertTrue(used, "no plan items in the example")
        self.assertEqual([a for a in used if a not in actions], [])

    def test_example_adds_up_and_is_ordered_like_the_cli(self):
        self.assertEqual(example_problems(parse_example(example_text(self.html))), [])

    def test_removal_rule_is_stated_precisely(self):
        flat = re.sub(r"\s+", " ", self.text)
        self.assertNotRegex(flat, r"(?i)only when all \w+ (?:facts )?are yes",
                            "a repo with some facts no can still be proposed for recover-backup-then-remove")
        para = re.search(r"all eleven are yes.*?</p>", re.sub(r"\s+", " ", self.html)).group(0)
        for needle in ("backup-then-remove", "recover-backup-then-remove", "unknown", "ask"):
            self.assertIn(needle, para)

    def test_class_defaults_say_what_todays_plan_does(self):
        d = class_table_defaults(self.html)
        self.assertEqual(len(d), 9, sorted(d))
        self.assertIn("Today's plan", self.html)
        self.assertIn("remove-regenerable", d["cache"])
        self.assertIn("Planned", d["cache"])
        self.assertIn("keep", d["output"].lower())
        self.assertIn("Planned", d["output"])
        self.assertIn("Planned", d["agent-session"])
        for cls, text in d.items():
            self.assertNotRegex(text, r"(?i)^remove by age", f"{cls}: an age or size policy is not built")

    def test_card_headings_do_not_state_planned_features_as_fact(self):
        self.assertNotIn("Everything removed is findable", self.html)
        self.assertIn("Everything removed will be findable", self.html)

    def test_brand_link_goes_to_the_document_top(self):
        page = parse_page()
        tops = [t for t, d in page.tags if d.get("id") == "top"]
        self.assertEqual(tops, ["body"])

    def test_phone_legend_does_not_rely_on_color_alone(self):
        svg = re.search(r'<figure>.*?</figure>', self.html, re.S).group(0)
        tiles = re.findall(r'<rect class="tile-([a-z]+)"', svg)
        keys = re.findall(r'<text class="tile-key"[^>]*>(\d+)</text>', svg)
        self.assertEqual(len(keys), len(tiles), "every tile needs a visible key at phone width")
        legend = re.findall(r'<li><span class="dot"[^>]*></span><span class="key">(\d+)</span>', svg)
        self.assertEqual(sorted(set(keys)), sorted(legend))
        self.assertRegex(self.html, r"@media \(max-width: 44rem\) \{[^}]*\.tile-key \{ display: inline; \}")

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

    def test_both_workflows_call_the_prepare_script(self):
        for path in (PAGES_YML, SITE_YML):
            self.assertIn("python3 site/tools/prepare.py _site", read(path), path.name)
        y = read(PAGES_YML)
        self.assertLess(y.index("site/tools/prepare.py"), y.index("upload-pages-artifact"))
        self.assertNotRegex(y, r"\brm\b", "pages.yml must not prune files itself; prepare.py does")
        self.assertRegex(y, r"upload-pages-artifact@[0-9a-f]{40}\s*\n\s*with:\s*\n\s*path: _site")

    def test_prepare_script_excludes_contributor_files(self):
        import importlib.util
        import tempfile
        spec = importlib.util.spec_from_file_location("prepare", SITE / "tools" / "prepare.py")
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            mod.prepare(out)
            for gone in ("README.md", "brand/BRAND.md", "tests", "tools"):
                self.assertFalse((out / gone).exists(), gone)
            for kept in ("index.html", "brand/logo.svg", "brand/logo-dark.svg", "brand/favicon.svg"):
                self.assertTrue((out / kept).is_file(), kept)

    def test_rehearsal_job_does_not_deploy(self):
        y = read(SITE_YML)
        self.assertIn("pages-rehearsal:", y)
        self.assertRegex(y, r"pages-rehearsal:\s*\n\s*needs: test")
        self.assertIn("/disk-cleaner", y)
        self.assertIn("site/tools/smoke.py", y)
        self.assertNotIn("deploy-pages", y)
        self.assertNotIn("configure-pages", y)
        for path in ("/README.md", "/brand/BRAND.md", "/tests/test_site.py"):
            self.assertRegex(y, r"expect 404 " + re.escape(path))
        for path in ("/brand/favicon.svg", "/brand/logo.svg", "/brand/logo-dark.svg"):
            self.assertRegex(y, r"expect 200 " + re.escape(path) + r" image/svg\+xml")

    def test_smoke_reference_parser(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location("smoke", SITE / "tools" / "smoke.py")
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        bad = '<link href="brand/x.svg"><a href="#a">a</a><a href="https://e.example/">e</a><img src="/root.png">'
        self.assertEqual(mod.relative_refs(bad), ["/root.png", "brand/x.svg"])
        real = mod.relative_refs(read(INDEX))
        self.assertIn("brand/favicon.svg", real)
        self.assertFalse([r for r in real if r.startswith("/")], "root-relative reference breaks a project subpath")

    def test_pages_does_not_deploy_on_pull_requests(self):
        y = read(PAGES_YML)
        self.assertNotIn("pull_request", y)
        self.assertIn("branches: [main]", y)


if __name__ == "__main__":
    unittest.main()
