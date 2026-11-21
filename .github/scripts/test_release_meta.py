"""Behavioural tests for release_meta.py and for the way release.yml calls it.

The git cases run real git against a throwaway bare "origin" in a temporary directory. The CI API is
a list of run objects shaped like the REST API's. Nothing here touches the network or GitHub.

Run: python3 -I -m unittest discover -s .github/scripts -p 'test_*.py' -v
"""

import io
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import release_meta as rm  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(HERE))
RELEASE_YML = os.path.join(ROOT, ".github", "workflows", "release.yml")
WORKFLOWS = os.path.join(ROOT, ".github", "workflows")
REPO = "example/project"

GIT_ENV = dict(os.environ)
GIT_ENV.update({
    "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
    "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid",
})


def sh(args, cwd):
    return subprocess.run(args, cwd=cwd, env=GIT_ENV, check=True, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, universal_newlines=True).stdout.strip()


def cargo(version):
    return '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "%s"\nedition = "2021"\n' % version


def read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


class Repo:
    """origin (bare) plus a clone; main has commit A (and later A2); side branch has B."""

    def __init__(self, version="0.1.0"):
        self.dir = tempfile.mkdtemp(prefix="release-guard-")
        self.origin = os.path.join(self.dir, "origin.git")
        self.work = os.path.join(self.dir, "work")
        sh(["git", "init", "-q", "--bare", "-b", "main", self.origin], self.dir)
        sh(["git", "init", "-q", "-b", "main", self.work], self.dir)
        sh(["git", "remote", "add", "origin", self.origin], self.work)
        self.A = self.commit("Cargo.toml", cargo(version))
        sh(["git", "push", "-q", "origin", "main"], self.work)
        sh(["git", "checkout", "-q", "-b", "side"], self.work)
        self.B = self.commit("side.txt", "side\n")
        sh(["git", "push", "-q", "origin", "side"], self.work)
        sh(["git", "checkout", "-q", "main"], self.work)

    def commit(self, name, text):
        with open(os.path.join(self.work, name), "w", encoding="utf-8") as f:
            f.write(text)
        sh(["git", "add", name], self.work)
        sh(["git", "commit", "-q", "-m", "c " + name], self.work)
        return sh(["git", "rev-parse", "HEAD"], self.work)

    def tag(self, name, commit, annotated=False, force=False):
        args = ["git", "tag"] + (["-a", "-m", "release"] if annotated else []) + (["-f"] if force else [])
        sh(args + [name, commit], self.work)
        sh(["git", "push", "-q"] + (["-f"] if force else []) + ["origin", "refs/tags/" + name], self.work)
        return sh(["git", "rev-parse", "refs/tags/" + name], self.work)

    def checkout(self, rev):
        sh(["git", "checkout", "-q", "--detach", rev], self.work)

    def cleanup(self):
        shutil.rmtree(self.dir, ignore_errors=True)


def run(sha, event="push", head_sha=None, conclusion="success", status="completed",
        path=".github/workflows/ci.yml", branch="main"):
    return {"event": event, "head_sha": head_sha or sha, "conclusion": conclusion, "status": status,
            "path": path, "head_branch": branch}


def tag_env(tag, sha):
    return {"EVENT": "push", "REF_TYPE": "tag", "REF_NAME": tag, "GITHUB_REF": "refs/tags/" + tag,
            "GITHUB_SHA": sha, "GITHUB_REPOSITORY": REPO, "RUN_ID": "77"}


class FakeGitHub:
    """In-memory releases API with real state, like REST v3: list, create, read, upload. It answers
    any other verb too (PATCH, DELETE) so a test sees it if the script ever sends one."""

    def __init__(self, fail_upload=None):
        self.releases = {}
        self.calls = []
        self.next_id = 1000
        self.fail_upload = fail_upload

    def api(self, base="https://api.github.com"):
        return rm.GitHubAPI("test-token", base, transport=self.transport)

    def create(self, body):
        self.next_id += 1
        rid = self.next_id
        rel = dict(body, id=rid, node_id="RE_%d" % rid, created_at="2026-11-21T00:00:%02dZ" % (rid % 60),
                   author={"login": "github-actions[bot]"}, assets=[],
                   html_url="https://github.com/%s/releases/tag/untagged-%d" % (REPO, rid),
                   upload_url="https://uploads.github.com/repos/%s/releases/%d/assets{?name,label}" % (REPO, rid))
        self.releases[rid] = rel
        return rel

    def verbs(self):
        return sorted(set(m for m, _ in self.calls))

    def transport(self, method, url, data, headers):
        import json as _j
        import urllib.parse as up
        assert headers["Authorization"] == "Bearer test-token"
        self.calls.append((method, url))
        u = up.urlparse(url)
        m = re.fullmatch(r"/repos/%s/releases(?:/(\d+))?(/assets)?" % re.escape(REPO), u.path)
        if not m:
            return 404, b'{"message":"Not Found"}'
        rid = int(m.group(1)) if m.group(1) else None
        if u.hostname == "uploads.github.com" and method == "POST" and m.group(2):
            name = up.parse_qs(u.query)["name"][0]
            if rid not in self.releases or name == self.fail_upload:
                return 500, b"{}"
            asset = {"name": name, "size": len(data)}
            self.releases[rid]["assets"].append(asset)
            return 201, _j.dumps(asset).encode()
        if u.hostname != "api.github.com":
            return 404, b"{}"
        if method == "GET" and rid is None:
            page = int(up.parse_qs(u.query).get("page", ["1"])[0])
            per = int(up.parse_qs(u.query).get("per_page", ["30"])[0])
            rels = sorted(self.releases.values(), key=lambda r: -r["id"])[(page - 1) * per:page * per]
            return 200, _j.dumps(rels).encode()
        if method == "POST" and rid is None:
            return 201, _j.dumps(self.create(_j.loads(data.decode()))).encode()
        if rid not in self.releases:
            return 404, b'{"message":"Not Found"}'
        if method == "GET":
            return 200, _j.dumps(self.releases[rid]).encode()
        if method == "DELETE":
            del self.releases[rid]
            return 204, b""
        if method == "PATCH":
            self.releases[rid].update(_j.loads(data.decode()))
            return 200, _j.dumps(self.releases[rid]).encode()
        return 405, b"{}"


def make_dist(d):
    dist = os.path.join(d, "dist")
    os.makedirs(dist)
    for n in ("disk-cleaner-v0.1.0-x86_64-unknown-linux-gnu.tar.gz", "disk-cleaner-v0.1.0-x86_64-pc-windows-msvc.zip"):
        with open(os.path.join(dist, n), "wb") as f:
            f.write(n.encode() * 3)
    rm.write_sums(dist)
    notes = os.path.join(d, "notes.md")
    with open(notes, "w") as f:
        f.write("PruneDisk v0.1.0 notes\n")
    return dist, notes


class TestCreateDraft(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp(prefix="release-create-")
        self.addCleanup(shutil.rmtree, self.d, True)
        self.dist, self.notes = make_dist(self.d)
        self.ident = os.path.join(self.d, "identity.json")
        self.summary = os.path.join(self.d, "summary.md")
        self.out = io.StringIO()

    def create(self, gh, tag="v0.1.0"):
        return rm.create_draft(gh.api(), REPO, tag, self.dist, self.notes, self.ident, out=self.out,
                               summary=self.summary)

    def refused(self, gh, text, tag="v0.1.0"):
        with self.assertRaises(rm.GuardError) as cm:
            self.create(gh, tag)
        self.assertIn(text, str(cm.exception))
        self.assertNotIn("DELETE", gh.verbs())
        self.assertNotIn("PATCH", gh.verbs())
        return str(cm.exception)

    def test_creates_a_draft_with_every_file_and_records_its_identity(self):
        gh = FakeGitHub()
        identity = self.create(gh)
        rel = gh.releases[identity["id"]]
        self.assertIs(rel["draft"], True)
        self.assertIs(rel["prerelease"], False)
        self.assertEqual(rel["name"], "PruneDisk v0.1.0")
        self.assertEqual(sorted(a["name"] for a in rel["assets"]), sorted(os.listdir(self.dist)))
        self.assertEqual(rel["assets"][-1]["name"], "SHA256SUMS")  # last: its presence marks a complete upload
        with open(self.ident) as f:
            import json as _j
            self.assertEqual(_j.load(f), identity)
        self.assertEqual(set(identity), set(rm.IDENTITY_KEYS))
        self.assertEqual(gh.verbs(), ["GET", "POST"])

    def test_suffix_makes_a_prerelease(self):
        gh = FakeGitHub()
        identity = self.create(gh, "v0.1.0-rc.1")
        self.assertIs(gh.releases[identity["id"]]["prerelease"], True)

    def test_failed_upload_leaves_the_draft_names_it_and_fails(self):
        gh = FakeGitHub(fail_upload="SHA256SUMS")
        keep = gh.create({"tag_name": "v0.0.9", "name": "old", "body": "", "draft": False, "prerelease": False})
        msg = self.refused(gh, "uploading SHA256SUMS failed")
        made = [r for r in gh.releases.values() if r["tag_name"] == "v0.1.0"]
        self.assertEqual(len(made), 1)
        self.assertIn(keep["id"], gh.releases)
        self.assertIn("release %d (%s) is a draft this run created" % (made[0]["id"], made[0]["html_url"]), msg)
        # The archives went up first; the draft lacks SHA256SUMS, which marks it incomplete.
        self.assertEqual(sorted(a["name"] for a in made[0]["assets"]),
                         sorted(n for n in os.listdir(self.dist) if n != "SHA256SUMS"))
        self.assertIn(msg, read(self.summary))

    def test_an_upload_that_did_not_land_fails_and_leaves_the_draft(self):
        # The upload answers 201 but the release does not hold the file when it is read back.
        gh = FakeGitHub()
        orig = gh.transport

        def lose_sums(method, url, data, headers):
            status, body = orig(method, url, data, headers)
            if method == "POST" and "name=SHA256SUMS" in url:
                for rel in gh.releases.values():
                    rel["assets"] = [a for a in rel["assets"] if a["name"] != "SHA256SUMS"]
            return status, body
        gh.transport = lose_sums
        self.refused(gh, "does not hold exactly the files built")
        self.assertEqual(len(gh.releases), 1)

    def test_a_release_for_the_tag_already_exists(self):
        # A re-run of a failed run, or a published release: never a second release for the tag.
        for draft in (True, False):
            with self.subTest(draft=draft):
                gh = FakeGitHub()
                for i in range(150):  # the existing release is on the second page
                    gh.create({"tag_name": "v0.0.%d" % i, "name": "", "body": "", "draft": False, "prerelease": False})
                old = gh.create({"tag_name": "v0.1.0", "name": "", "body": "", "draft": draft, "prerelease": False})
                for i in range(150):
                    gh.create({"tag_name": "v0.2.%d" % i, "name": "", "body": "", "draft": False, "prerelease": False})
                n = len(gh.releases)
                self.refused(gh, "a release for v0.1.0 already exists (id %d, %s)"
                             % (old["id"], "draft" if draft else "published"))
                self.assertEqual(len(gh.releases), n)
                self.assertEqual([m for m, _ in gh.calls], ["GET"] * 4)

    def test_a_file_changed_after_packaging_is_not_uploaded(self):
        name = [n for n in sorted(os.listdir(self.dist)) if n != "SHA256SUMS"][0]
        with open(os.path.join(self.dist, name), "ab") as f:
            f.write(b"tampered")
        gh = FakeGitHub()
        self.refused(gh, "does not match its line in SHA256SUMS")
        self.assertEqual(gh.calls, [])

    def test_sums_must_list_exactly_the_files(self):
        with open(os.path.join(self.dist, "extra.tar.gz"), "wb") as f:
            f.write(b"x")
        gh = FakeGitHub()
        self.refused(gh, "SHA256SUMS lists")
        self.assertEqual(gh.calls, [])

    def test_token_never_goes_to_another_host(self):
        gh = FakeGitHub()
        orig = gh.create

        def evil(body):
            rel = orig(body)
            rel["upload_url"] = "https://uploads.example.invalid/x{?name}"
            return rel
        gh.create = evil
        self.refused(gh, "refusing to send the token")
        self.assertFalse(any("example.invalid" in u for _, u in gh.calls))

    def test_refuses_stray_files(self):
        os.mkdir(os.path.join(self.dist, "dist"))
        with self.assertRaises(rm.GuardError):
            self.create(FakeGitHub())


class TestHTTPClient(unittest.TestCase):
    """The one HTTP client: GET and POST only, HTTPS only, allowed hosts only, no redirects."""

    def test_only_get_and_post(self):
        gh = FakeGitHub()
        for verb in ("DELETE", "PATCH", "PUT"):
            with self.assertRaises(rm.GuardError) as cm:
                gh.api().call(verb, "/repos/%s/releases/1" % REPO)
            self.assertIn("never edits, publishes or deletes", str(cm.exception))
        self.assertEqual(gh.calls, [])

    def test_https_and_allowed_hosts_only(self):
        gh = FakeGitHub()
        for url in ("http://api.github.com/x", "https://example.invalid/x", "https://api.github.com.example.invalid/x"):
            with self.assertRaises(rm.GuardError):
                gh.api().call("GET", url)
        self.assertEqual(gh.calls, [])

    def redirected(self, location):
        """Real urllib opener; the network layer is replaced by one that answers 302 once."""
        import email.message
        import urllib.request
        import urllib.response
        seen = []

        def https_open(handler, req):
            seen.append((req.full_url, req.get_header("Authorization")))
            hdrs = email.message.Message()
            first = len(seen) == 1
            if first:
                hdrs["Location"] = location
            resp = urllib.response.addinfourl(io.BytesIO(b"{}"), hdrs, req.full_url, 302 if first else 200)
            resp.msg = "Found" if first else "OK"
            return resp
        orig = urllib.request.HTTPSHandler.https_open
        urllib.request.HTTPSHandler.https_open = https_open
        try:
            with self.assertRaises(rm.GuardError) as cm:
                rm.GitHubAPI("secret-token").call("GET", "/repos/%s/releases?page=1" % REPO)
        finally:
            urllib.request.HTTPSHandler.https_open = orig
        return str(cm.exception), seen

    def test_a_redirect_to_another_host_is_not_followed(self):
        msg, seen = self.redirected("https://storage.example.invalid/x")
        self.assertIn("HTTP 302 redirect to storage.example.invalid refused", msg)
        self.assertNotIn("secret-token", msg)
        self.assertEqual([u for u, _ in seen], ["https://api.github.com/repos/%s/releases?page=1" % REPO])

    def test_a_redirect_on_an_allowed_host_is_not_followed_either(self):
        msg, seen = self.redirected("https://uploads.github.com/x")
        self.assertIn("redirect to uploads.github.com refused", msg)
        self.assertEqual(len(seen), 1)

    def test_ci_runs_lookup_uses_the_same_client(self):
        import urllib.request
        sent = []

        def https_open(handler, req):
            import urllib.response
            sent.append(req.get_header("Authorization"))
            resp = urllib.response.addinfourl(io.BytesIO(b'{"workflow_runs": []}'), {}, req.full_url, 200)
            resp.msg = "OK"
            return resp
        orig = urllib.request.HTTPSHandler.https_open
        urllib.request.HTTPSHandler.https_open = https_open
        try:
            self.assertEqual(rm.fetch_ci_runs(REPO, "a" * 40, "tok"), [])
        finally:
            urllib.request.HTTPSHandler.https_open = orig
        self.assertEqual(sent, ["Bearer tok"])
        # An error answer is an error, not "no runs".
        rm_api = rm.GitHubAPI
        try:
            rm.GitHubAPI = lambda token, api: type("E", (), {"call": lambda self, m, u: (500, {"message": "x"})})()
            with self.assertRaises(rm.GuardError):
                rm.fetch_ci_runs(REPO, "a" * 40, "tok")
        finally:
            rm.GitHubAPI = rm_api
        # The token is attached in exactly one place in the script.
        self.assertEqual(read(os.path.join(HERE, "release_meta.py")).count('"Authorization"'), 1)


class TestValidation(unittest.TestCase):
    GOOD = ["v0.1.0", "v1.2.3", "v10.20.30", "v0.1.0-rc.1", "v0.1.0-rehearsal.123", "v0.1.0-beta"]
    BAD = ["0.1.0", "v0.1", "v0.1.0-", "v0.1.0+build", "v0.1.0-rc-1", "v0.1.0-rc_1",
           "v0.1.0-';whoami;#", "v0.1.0-$(id)", "v0.1.0-`id`", "v0.1.0-a|e;#", "v0.1.0-x\nfoo",
           "v0.1.0\n", " v0.1.0", "v0.1.0-a/b", "v0.1.0-a b", "v٠.1.0", "V0.1.0", "",
           "v0.1.0-" + "a" * 70]

    def test_good_tags(self):
        for t in self.GOOD:
            self.assertEqual(rm.validate_tag(t), t)

    def test_bad_tags_rejected(self):
        for t in self.BAD:
            with self.assertRaises(rm.GuardError, msg=repr(t)):
                rm.validate_tag(t)

    def test_workspace_version(self):
        self.assertEqual(rm.workspace_version(cargo("0.3.1")), "0.3.1")
        with self.assertRaises(rm.GuardError):
            rm.workspace_version('[package]\nversion = "0.1.0"\n')
        with self.assertRaises(rm.GuardError):
            rm.workspace_version(cargo("0.1.0'; id"))

    def test_rehearsal_tag_passes_the_same_regex(self):
        self.assertEqual(rm.rehearsal_tag("0.1.0", "123"), "v0.1.0-rehearsal.123")
        self.assertEqual(rm.rehearsal_tag("0.2.0-rc.1", "5"), "v0.2.0-rc.1.rehearsal.5")
        with self.assertRaises(rm.GuardError):
            rm.rehearsal_tag("0.1.0", "12;id")

    def test_the_real_cargo_toml_has_a_valid_version(self):
        rm.workspace_version(read(os.path.join(ROOT, "Cargo.toml")))


class TestGreenCI(unittest.TestCase):
    SHA = "a" * 40

    def test_push_success_counts(self):
        self.assertIsNotNone(rm.green_push_run([run(self.SHA)], self.SHA))

    def test_side_branch_push_of_the_same_commit_counts(self):
        self.assertIsNotNone(rm.green_push_run([run(self.SHA, branch="feat/x")], self.SHA))

    def test_pull_request_only_does_not_count(self):
        self.assertIsNone(rm.green_push_run([run(self.SHA, event="pull_request")], self.SHA))

    def test_failed_running_other_commit_other_workflow_do_not_count(self):
        runs = [run(self.SHA, conclusion="failure"), run(self.SHA, status="in_progress", conclusion=None),
                run(self.SHA, head_sha="b" * 40), run(self.SHA, path=".github/workflows/site.yml"),
                run(self.SHA, event="workflow_dispatch")]
        self.assertIsNone(rm.green_push_run(runs, self.SHA))

    def test_none(self):
        self.assertIsNone(rm.green_push_run([], self.SHA))


class TestMetaWithGit(unittest.TestCase):
    def setUp(self):
        self.r = Repo()
        self.addCleanup(self.r.cleanup)
        self.out = io.StringIO()

    def meta(self, env, runs):
        self.r.checkout(env["GITHUB_SHA"])
        return rm.meta(env, cwd=self.r.work, fetch_runs=lambda repo, sha: runs, out=self.out)

    def refused(self, env, runs, needle):
        with self.assertRaises(rm.GuardError) as cm:
            self.meta(env, runs)
        self.assertIn(needle, str(cm.exception))
        return str(cm.exception)

    def test_good_lightweight_tag(self):
        self.r.tag("v0.1.0", self.r.A)
        out = self.meta(tag_env("v0.1.0", self.r.A), [run(self.r.A)])
        self.assertEqual(out, {"tag": "v0.1.0", "version": "0.1.0", "sha": self.r.A, "publish": "true"})

    def test_good_annotated_tag_given_as_the_tag_object(self):
        obj = self.r.tag("v0.1.0", self.r.A, annotated=True)
        self.assertNotEqual(obj, self.r.A)
        out = self.meta(tag_env("v0.1.0", obj), [run(self.r.A)])
        self.assertEqual(out["sha"], self.r.A)
        self.assertEqual(out["publish"], "true")

    def test_good_annotated_tag_given_as_the_commit(self):
        self.r.tag("v0.1.0", self.r.A, annotated=True)
        self.assertEqual(self.meta(tag_env("v0.1.0", self.r.A), [run(self.r.A)])["publish"], "true")

    def test_wrong_version(self):
        self.r.tag("v0.2.0", self.r.A)
        self.refused(tag_env("v0.2.0", self.r.A), [run(self.r.A)], "not v + the workspace version")

    def test_suffix_must_be_in_cargo_version(self):
        self.r.tag("v0.1.0-rc.1", self.r.A)
        self.refused(tag_env("v0.1.0-rc.1", self.r.A), [run(self.r.A)], "not v + the workspace version")

    def test_bad_suffix_refused_before_anything_else(self):
        env = tag_env("v0.1.0-rc_1", self.r.A)
        calls = []
        self.r.checkout(self.r.A)
        with self.assertRaises(rm.GuardError) as cm:
            rm.meta(env, cwd=self.r.work, fetch_runs=lambda *a: calls.append(a) or [], out=self.out)
        self.assertIn("is not vMAJOR", str(cm.exception))
        self.assertEqual(calls, [], "the CI API was asked about an invalid tag")

    def test_injection_shaped_tag_refused(self):
        # A valid git ref name; it must never reach a shell or PowerShell.
        tag = "v0.1.0-';whoami;#"
        self.assertEqual(subprocess.run(["git", "check-ref-format", "refs/tags/" + tag]).returncode, 0)
        self.r.tag(tag, self.r.A)
        self.refused(tag_env(tag, self.r.A), [run(self.r.A)], "is not vMAJOR")

    def test_off_main(self):
        self.r.tag("v0.1.0", self.r.B)
        self.refused(tag_env("v0.1.0", self.r.B), [run(self.r.B, branch="side")], "is not on main")

    def test_no_green_ci(self):
        self.r.tag("v0.1.0", self.r.A)
        self.refused(tag_env("v0.1.0", self.r.A), [], "no successful ci.yml run")
        self.refused(tag_env("v0.1.0", self.r.A), [run(self.r.A, conclusion="failure")], "no successful ci.yml run")

    def test_pull_request_only_ci(self):
        self.r.tag("v0.1.0", self.r.A)
        self.refused(tag_env("v0.1.0", self.r.A), [run(self.r.A, event="pull_request")], "pull_request runs")

    def test_moved_tag(self):
        self.r.tag("v0.1.0", self.r.A)
        a2 = self.r.commit("more.txt", "x\n")
        sh(["git", "push", "-q", "origin", "main"], self.r.work)
        self.r.tag("v0.1.0", a2, force=True)
        msg = self.refused(tag_env("v0.1.0", self.r.A), [run(self.r.A)], "it was moved")
        self.assertIn(a2, msg)

    def test_deleted_tag(self):
        self.r.tag("v0.1.0", self.r.A)
        sh(["git", "push", "-q", "origin", ":refs/tags/v0.1.0"], self.r.work)
        self.refused(tag_env("v0.1.0", self.r.A), [run(self.r.A)], "no longer exists")

    def test_ref_must_match_tag(self):
        self.r.tag("v0.1.0", self.r.A)
        env = tag_env("v0.1.0", self.r.A)
        env["GITHUB_REF"] = "refs/heads/main"
        self.refused(env, [run(self.r.A)], "does not match the tag name")

    def test_rehearsal_from_branch(self):
        env = {"EVENT": "push", "REF_TYPE": "branch", "REF_NAME": "feat/x", "GITHUB_REF": "refs/heads/feat/x",
               "GITHUB_SHA": self.r.B, "GITHUB_REPOSITORY": REPO, "RUN_ID": "991"}
        out = self.meta(env, [run(self.r.B, branch="feat/x")])
        self.assertEqual(out["publish"], "false")
        self.assertEqual(out["tag"], "v0.1.0-rehearsal.991")
        self.assertIn("would be refused", self.out.getvalue())
        self.assertIn("is not on main", self.out.getvalue())

    def test_dispatch_on_a_tag_ref_is_refused(self):
        self.r.tag("v0.1.0", self.r.A)
        env = {"EVENT": "workflow_dispatch", "REF_TYPE": "tag", "REF_NAME": "v0.1.0",
               "GITHUB_REF": "refs/tags/v0.1.0", "GITHUB_SHA": self.r.A, "GITHUB_REPOSITORY": REPO,
               "RUN_ID": "3"}
        self.refused(env, [run(self.r.A)], "a rehearsal runs from a branch")


class TestBinding(unittest.TestCase):
    def setUp(self):
        self.r = Repo()
        self.addCleanup(self.r.cleanup)
        self.out = io.StringIO()

    def test_same_commit_passes_lightweight_and_annotated(self):
        self.r.tag("v0.1.0", self.r.A)
        rm.check_binding("v0.1.0", self.r.A, cwd=self.r.work, out=self.out)
        self.r.tag("v0.1.1", self.r.A, annotated=True)
        rm.check_binding("v0.1.1", self.r.A, cwd=self.r.work, out=self.out)

    def test_moved_tag_fails(self):
        self.r.tag("v0.1.0", self.r.A)
        self.r.tag("v0.1.0", self.r.B, force=True)
        with self.assertRaises(rm.GuardError):
            rm.check_binding("v0.1.0", self.r.A, cwd=self.r.work, out=self.out)

    def test_missing_tag_fails(self):
        with self.assertRaises(rm.GuardError):
            rm.check_binding("v0.1.0", self.r.A, cwd=self.r.work, out=self.out)

    def made_draft(self):
        d = tempfile.mkdtemp(prefix="release-draft-")
        self.addCleanup(shutil.rmtree, d, True)
        gh = FakeGitHub()
        dist, notes = make_dist(d)
        ident = os.path.join(d, "identity.json")
        identity = rm.create_draft(gh.api(), REPO, "v0.1.0", dist, notes, identity_path=ident, out=self.out)
        return gh, ident, identity, os.path.join(d, "summary.md")

    def test_after_create_leaves_the_draft_names_it_and_fails(self):
        # Whatever happened to the draft meanwhile (untouched, edited, published, replaced), the
        # check only reads the tag: it makes no API call, so it cannot delete or change anything.
        for change in ("none", "edited", "published", "replaced"):
            with self.subTest(change=change):
                gh, ident, identity, summary = self.made_draft()
                rel = gh.releases[identity["id"]]
                if change == "edited":
                    rel.update(name="edited", body="edited", prerelease=True, assets=rel["assets"][:1])
                elif change == "published":
                    rel["draft"] = False
                elif change == "replaced":
                    del gh.releases[identity["id"]]
                    gh.create({"tag_name": "v0.1.0", "name": "x", "body": "", "draft": True, "prerelease": False})
                before = dict((k, dict(v)) for k, v in gh.releases.items())
                n = len(gh.calls)
                if change == "none":
                    self.r.tag("v0.1.0", self.r.B)
                with self.assertRaises(rm.GuardError) as cm:
                    rm.check_binding("v0.1.0", self.r.A, cwd=self.r.work, after_create=True, repo=REPO,
                                     identity_path=ident, out=self.out, summary=summary)
                msg = str(cm.exception)
                self.assertEqual(len(gh.calls), n)
                self.assertEqual(gh.releases, before)
                self.assertIn("release %d (%s)" % (identity["id"], identity["html_url"]), msg)
                self.assertIn("left in place and nothing was deleted", msg)
                self.assertIn("repos/%s/releases/%d --jq .draft" % (REPO, identity["id"]), msg)
                self.assertIn(msg, read(summary))

    def test_after_create_with_the_tag_in_place_passes(self):
        gh, ident, _, summary = self.made_draft()
        self.r.tag("v0.1.0", self.r.A)
        rm.check_binding("v0.1.0", self.r.A, cwd=self.r.work, after_create=True, repo=REPO,
                         identity_path=ident, out=self.out, summary=summary)
        self.assertFalse(os.path.exists(summary))

    def test_invalid_inputs_never_reach_git(self):
        for tag, sha in (("v0.1.0-';id;#", "a" * 40), ("v0.1.0", "HEAD"), ("v0.1.0", "-a" * 20)):
            with self.assertRaises(rm.GuardError):
                rm.check_binding(tag, sha, cwd=self.r.work, out=self.out)


class TestPackageAndAttestations(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp(prefix="release-dist-")
        self.addCleanup(shutil.rmtree, self.d, True)
        self.dist = os.path.join(self.d, "dist")
        os.mkdir(self.dist)
        for n in ("b-x86_64.zip", "a-linux.tar.gz", "app.dmg"):
            with open(os.path.join(self.dist, n), "wb") as f:
                f.write(n.encode())
        self.out = io.StringIO()

    def test_sums_cover_every_file_sorted(self):
        rm.write_sums(self.dist)
        names = [l.split("  ")[1].strip() for l in read(os.path.join(self.dist, "SHA256SUMS")).splitlines()]
        self.assertEqual(names, ["a-linux.tar.gz", "app.dmg", "b-x86_64.zip"])
        rm.write_sums(self.dist)  # SHA256SUMS itself is never listed
        self.assertEqual(rm.published_files(self.dist), names)

    def test_notes_render_and_match_the_published_command(self):
        tpl = os.path.join(ROOT, ".github", "release-notes.md")
        notes_out = os.path.join(self.d, "notes.md")
        rm.package(self.dist, tpl, notes_out, "v0.1.0", "0.1.0", "c" * 40, "disk-cleaner", REPO)
        notes = read(notes_out)
        self.assertIn(rm.verify_command("<archive file name>", REPO, "v0.1.0"), notes)
        self.assertIn("c" * 40, notes)
        self.assertNotRegex(notes, r"@[A-Z_]+@")

    def test_a_directory_in_dist_fails_closed(self):
        os.mkdir(os.path.join(self.dist, "dist"))
        with self.assertRaises(rm.GuardError):
            rm.write_sums(self.dist)

    def test_unknown_placeholder_fails(self):
        with self.assertRaises(rm.GuardError):
            rm.render_notes("@BIN@ @NEW@", {k: "x" for k in rm.PLACEHOLDERS})

    def test_package_validates_inputs(self):
        with self.assertRaises(rm.GuardError):
            rm.package(self.dist, "x", "y", "v0.1.0-';id;#", "0.1.0", "c" * 40, "disk-cleaner", REPO)

    def fake_gh(self, attested_ref):
        """gh attestation verify: passes only when --source-ref equals the ref the build ran on."""
        calls = []

        def run(args):
            calls.append(args)
            self.assertEqual(args[:3], ["gh", "attestation", "verify"])
            self.assertEqual(args[args.index("--signer-workflow") + 1], REPO + "/.github/workflows/release.yml")
            return 0 if args[args.index("--source-ref") + 1] == attested_ref else 1
        return run, calls

    def test_rehearsal_artifacts_fail_the_published_command(self):
        rm.write_sums(self.dist)
        run, calls = self.fake_gh("refs/heads/feat/x")
        rm.check_attestations(self.dist, REPO, "v0.1.0-rehearsal.5", "false", "refs/heads/feat/x",
                              run=run, sleep=lambda s: None, out=self.out)
        self.assertEqual(len(calls), 6)  # control + published, for each of the three files

    def test_rehearsal_check_fails_if_branch_build_would_pass(self):
        # A broken variant: the published command not pinned to the tag (the 4ed3076 form).
        rm.write_sums(self.dist)
        run = lambda args: 0  # noqa: E731  every command passes, as --repo alone would
        with self.assertRaises(rm.GuardError):
            rm.check_attestations(self.dist, REPO, "v0.1.0-rehearsal.5", "false", "refs/heads/feat/x",
                                  run=run, sleep=lambda s: None, out=self.out)

    def test_rehearsal_check_fails_without_any_attestation(self):
        rm.write_sums(self.dist)
        with self.assertRaises(rm.GuardError):
            rm.check_attestations(self.dist, REPO, "v0.1.0-rehearsal.5", "false", "refs/heads/feat/x",
                                  run=lambda a: 1, sleep=lambda s: None, out=self.out)

    def test_release_passes_only_for_the_tag(self):
        rm.write_sums(self.dist)
        run, _ = self.fake_gh("refs/tags/v0.1.0")
        rm.check_attestations(self.dist, REPO, "v0.1.0", "true", "refs/tags/v0.1.0", run=run,
                              sleep=lambda s: None, out=self.out)
        run, _ = self.fake_gh("refs/heads/main")
        with self.assertRaises(rm.GuardError):
            rm.check_attestations(self.dist, REPO, "v0.1.0", "true", "refs/tags/v0.1.0", run=run,
                                  sleep=lambda s: None, out=self.out)


def step_script(text, job, contains):
    jobs = rm.split_jobs(text)
    for step in rm.split_steps(jobs[job]):
        for _, s in rm.run_blocks(step):
            if contains in s:
                return step, s
    raise AssertionError("no %s step containing %r" % (job, contains))


class TestWorkflow(unittest.TestCase):
    """release.yml as written. Each mutation is a way a guard has been, or could be, bypassed."""

    @classmethod
    def setUpClass(cls):
        cls.y = read(RELEASE_YML)

    def test_release_yml_is_clean(self):
        self.assertEqual(rm.lint_workflow(self.y), [])

    def test_parser_sees_the_scripts(self):
        self.assertGreaterEqual(len(rm.run_blocks(self.y)), 12)
        self.assertEqual(set(rm.GATED_JOBS) - set(rm.split_jobs(self.y)), set())

    def test_no_untrusted_interpolation_in_any_workflow(self):
        for name in sorted(os.listdir(WORKFLOWS)):
            if name.endswith((".yml", ".yaml")):
                text = read(os.path.join(WORKFLOWS, name))
                self.assertEqual(rm.lint_workflow(text, strict=(name == "release.yml")), [], name)

    MUTATIONS = [
        ("|| true after the guard", "release_meta.py meta\n", "release_meta.py meta || true\n"),
        ("|| : after the binding check", "release_meta.py check-binding\n", "release_meta.py check-binding || :\n"),
        ("guard removed", "run: python3 -I .github/scripts/release_meta.py meta\n", "run: echo skipped\n"),
        ("guard made conditional", "        run: python3 -I .github/scripts/release_meta.py check-binding\n",
         "        run: python3 -I .github/scripts/release_meta.py check-binding\n        continue-on-error: true\n"),
        ("tests removed", "python3 -I -m unittest discover", "echo"),
        ("after-create check removed", "release_meta.py check-binding --after-create", "echo done"),
        ("attestation check removed", "release_meta.py check-attestations", "echo"),
        ("continue-on-error on meta", "  meta:\n", "  meta:\n    continue-on-error: true\n"),
        ("build does not need meta", "    needs: meta\n", ""),
        ("draft after failure", "    if: needs.meta.outputs.publish == 'true'\n",
         "    if: always()\n"),
        ("matrix value interpolated (release.yml allows no ${{ }} in scripts at all)",
         'cargo build --release --locked --target "$TARGET"', "cargo build --release --locked --target ${{ matrix.target }}"),
        ("tag interpolated", 'name="${BIN_NAME}-${TAG}-${TARGET}"', 'name="${BIN_NAME}-${{ needs.meta.outputs.tag }}-${TARGET}"'),
        ("create step removed", "run: python3 -I .github/scripts/release_meta.py create-draft\n", "run: echo created\n"),
        ("release deleted by name", "run: python3 -I .github/scripts/release_meta.py check-binding --after-create\n",
         "run: python3 -I .github/scripts/release_meta.py check-binding --after-create\n"
         "      - name: cleanup\n        run: gh release delete \"$TAG\" --yes\n"),
        ("token given to the after-create check", "      - name: The tag did not move while the draft was made\n"
         "        env:\n", "      - name: The tag did not move while the draft was made\n        env:\n"
         "          GH_TOKEN: ${{ github.token }}\n"),
        ("release deleted by id through the API",
         "run: python3 -I .github/scripts/release_meta.py check-binding --after-create\n",
         "run: python3 -I .github/scripts/release_meta.py check-binding --after-create\n"
         "      - name: cleanup\n        run: curl -X DELETE \"$API/repos/$REPO/releases/$ID\"\n"),
        ("release created with gh", "run: python3 -I .github/scripts/release_meta.py create-draft\n",
         "run: gh release create \"$TAG\" --draft out/dist/*\n"),
        ("PowerShell command string", "Compress-Archive -LiteralPath $name",
         "Compress-Archive -LiteralPath $name\n          powershell -NoProfile -Command \"Compress-Archive -Path '$name'\""),
        ("sed program", "set -euo pipefail\n", "set -euo pipefail\n          sed -e \"s|@TAG@|$TAG|g\" x\n"),
        ("build of another ref", "ref: ${{ needs.meta.outputs.sha }}", "ref: ${{ github.ref }}"),
        ("installers not attested", "subject-checksums: dist/SHA256SUMS", "subject-path: dist/*.zip"),
        ("bash not default", "defaults:\n  run:\n    shell: bash\n", ""),
        ("guard step skipped with if: false", "      - id: meta\n", "      - id: meta\n        if: false\n"),
        ("binding step in another directory", "      - name: The tag still points at the built commit\n",
         "      - name: The tag still points at the built commit\n        working-directory: out\n"),
    ]

    def test_every_mutation_is_caught(self):
        for what, old, new in self.MUTATIONS:
            self.assertIn(old, self.y, "mutation %r no longer applies; update it" % what)
            mutated = self.y.replace(old, new, 1)
            self.assertNotEqual(rm.lint_workflow(mutated), [], "not caught: " + what)

    def run_step(self, script, stub_exit, env):
        d = tempfile.mkdtemp(prefix="release-step-")
        self.addCleanup(shutil.rmtree, d, True)
        os.makedirs(os.path.join(d, ".github", "scripts"))
        with open(os.path.join(d, ".github", "scripts", "release_meta.py"), "w") as f:
            f.write("import sys\nprint('stub', sys.argv[1:])\nsys.exit(%d)\n" % stub_exit)
        path = os.path.join(d, "step.sh")
        with open(path, "w") as f:
            f.write(script + "\n")
        full = dict(os.environ)
        full.update(env)
        # The shell GitHub uses for shell: bash.
        return subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", path], cwd=d, env=full,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)

    def test_guard_steps_fail_the_job_when_the_guard_fails(self):
        for job, needle in (("meta", "release_meta.py meta"), ("draft-release", "check-binding\n"),
                            ("draft-release", "--after-create"), ("draft-release", "create-draft"),
                            ("package", "check-attestations")):
            _, script = step_script(self.y + "\n", job, needle.strip())
            self.assertNotEqual(self.run_step(script, 1, {}).returncode, 0, (job, needle))
            self.assertEqual(self.run_step(script, 0, {}).returncode, 0, (job, needle))

    def test_the_zip_step_treats_the_name_as_data(self):
        pwsh = shutil.which("pwsh")
        if not pwsh:
            if os.environ.get("REQUIRE_PWSH") == "1":
                self.fail("pwsh is required here")
            self.skipTest("pwsh not installed")
        step, script = step_script(self.y, "build", "Compress-Archive")
        self.assertRegex(step, r"shell: pwsh")
        d = tempfile.mkdtemp(prefix="release-zip-")
        self.addCleanup(shutil.rmtree, d, True)
        # If the name were parsed as code this would print INJECTED, a word the name itself does not
        # contain (the step lists the directory, so the name is printed either way).
        tag = "v0.1.0-';Write-Output ('INJ'+'ECTED');#"
        name = "disk-cleaner-%s-x86_64-pc-windows-msvc" % tag
        os.makedirs(os.path.join(d, "dist", name))
        with open(os.path.join(d, "dist", name, "README.md"), "w") as f:
            f.write("x\n")
        env = dict(os.environ, BIN_NAME="disk-cleaner", TAG=tag, TARGET="x86_64-pc-windows-msvc")
        path = os.path.join(d, "step.ps1")
        with open(path, "w") as f:
            f.write("$ErrorActionPreference = 'Stop'\n" + script + "\n")
        p = subprocess.run([pwsh, "-NoProfile", "-NonInteractive", "-File", path], cwd=d, env=env,
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)
        self.assertEqual(p.returncode, 0, p.stdout)
        self.assertNotIn("INJECTED", p.stdout)
        self.assertTrue(os.path.isfile(os.path.join(d, "dist", name + ".zip")), os.listdir(os.path.join(d, "dist")))
        import zipfile
        with zipfile.ZipFile(os.path.join(d, "dist", name + ".zip")) as z:
            self.assertTrue(any(n.replace("\\", "/").endswith(name + "/README.md") for n in z.namelist()), z.namelist())


class TestPublishedText(unittest.TestCase):
    """Every gh attestation verify a reader can copy is the tag-pinned command, nothing weaker."""

    FILES = ["README.md", "site/index.html", "docs/RELEASING.md", ".github/release-notes.md"]

    def commands(self, rel):
        text = read(os.path.join(ROOT, rel))
        if rel.endswith(".html"):
            text = re.sub(r"<[^>]+>", "", text).replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
        # Every occurrence followed by an argument, i.e. a command someone can copy (prose that only
        # names `gh attestation verify` in code font is followed by a backtick and is skipped).
        return [re.sub(r"\s+", " ", m.group(0)).strip()
                for m in re.finditer(r"gh attestation verify[ \t]+[^\s`][^\n`]*", text)]

    def test_every_published_command_is_pinned(self):
        seen = 0
        for rel in self.FILES:
            cmds = self.commands(rel)
            self.assertTrue(cmds, rel + " shows no verification command")
            for c in cmds:
                seen += 1
                tag = re.search(r"--source-ref refs/tags/(\S+)", c)
                self.assertIsNotNone(tag, "%s: %r is not pinned to a tag" % (rel, c))
                repo = "@REPO@" if "@REPO@" in c else "ericmacdougall/disk-cleaner"
                want = rm.verify_command("<archive file name>", repo, tag.group(1))
                self.assertEqual(c.split(" #")[0].strip(), want, rel)
        self.assertGreaterEqual(seen, 4)


if __name__ == "__main__":
    unittest.main()
