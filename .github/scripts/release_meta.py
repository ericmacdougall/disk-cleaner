#!/usr/bin/env python3
"""Release guards for .github/workflows/release.yml, in one place so they can be tested.

Every value that comes from a ref name (the tag) passes validate_tag() before anything uses it, and
reaches git and gh only as a separate argument (never through a shell). The workflow calls this
script; it never builds a command line out of the tag itself.

Subcommands (all read their inputs from the environment the workflow sets, see each function):

  meta               decide tag, version, commit and whether this run may create a draft release
  package            write dist/SHA256SUMS and fill in the release notes
  check-attestations run the published verification command against every file in SHA256SUMS
  check-binding      the tag on the remote still points at the commit that was built (with
                     --after-create: if not, fail and name the draft this run made; it is
                     never deleted)
  create-draft       create the draft release, upload every file, record the release's identity

Nothing here edits, publishes or deletes a release: the one HTTP client sends GET and POST only,
over HTTPS, to GitHub's API and upload hosts, and never follows a redirect.
  lint-workflow      static checks over release.yml (also run by the tests)

These guards are hygiene. They run from the workflow file in the tagged commit, so someone who can
push a tag can also push a commit whose release.yml leaves them out. The real controls are the
repository settings and the maintainer's publish step described in docs/RELEASING.md.

Python 3.9 compatible, standard library only.
"""

import argparse
import contextlib
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

TAG_RE = re.compile(r"v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?")
VERSION_RE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?")
SHA_RE = re.compile(r"[0-9a-f]{40}")
REPO_RE = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
NAME_RE = re.compile(r"[A-Za-z0-9._+-]+")
RUN_ID_RE = re.compile(r"[0-9]+")
MAX_TAG_LEN = 64
RELEASE_WORKFLOW = ".github/workflows/release.yml"
CI_WORKFLOW_FILE = "ci.yml"
CI_WORKFLOW_PATH = ".github/workflows/ci.yml"
SUMS = "SHA256SUMS"
IDENTITY_FILE = "release-identity.json"
PLACEHOLDERS = ("BIN", "TAG", "VERSION", "REPO", "SHA")


class GuardError(Exception):
    """A release guard refused. The message says why."""


# ---------------------------------------------------------------------------------------------
# Validation. These are the only ways a tag, version, commit or repository name enters the rest.


def validate_tag(tag):
    if not isinstance(tag, str) or len(tag) > MAX_TAG_LEN or not TAG_RE.fullmatch(tag):
        raise GuardError(
            "tag %r is not vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-SUFFIX "
            "(suffix: ASCII letters, digits and dots only)" % (tag,))
    return tag


def validate_version(version):
    if not isinstance(version, str) or not VERSION_RE.fullmatch(version):
        raise GuardError("workspace version %r is not MAJOR.MINOR.PATCH[-SUFFIX]" % (version,))
    return version


def validate_sha(sha):
    if not isinstance(sha, str) or not SHA_RE.fullmatch(sha):
        raise GuardError("%r is not a full 40-character commit id" % (sha,))
    return sha


def validate_repo(repo):
    if not isinstance(repo, str) or not REPO_RE.fullmatch(repo):
        raise GuardError("%r is not owner/name" % (repo,))
    return repo


def is_prerelease(tag):
    return "-" in validate_tag(tag)


def workspace_version(cargo_toml_text):
    """version from the [workspace.package] table of Cargo.toml (no TOML library on Python 3.9)."""
    in_table = False
    for line in cargo_toml_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_table = stripped == "[workspace.package]"
            continue
        if in_table:
            m = re.match(r'version\s*=\s*"([^"]*)"\s*(#.*)?$', stripped)
            if m:
                return validate_version(m.group(1))
    raise GuardError("could not read version from the [workspace.package] table of Cargo.toml")


def rehearsal_tag(version, run_id):
    validate_version(version)
    if not isinstance(run_id, str) or not RUN_ID_RE.fullmatch(run_id):
        raise GuardError("run id %r is not a number" % (run_id,))
    sep = "." if "-" in version else "-"
    return validate_tag("v%s%srehearsal.%s" % (version, sep, run_id))


# ---------------------------------------------------------------------------------------------
# git and the GitHub API. Arguments are lists; nothing goes through a shell.


def git(args, cwd=None):
    proc = subprocess.run(["git"] + list(args), cwd=cwd, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, universal_newlines=True)
    return proc.returncode, proc.stdout, proc.stderr


def peel_local(rev, cwd=None):
    """The commit that rev (a commit id or an annotated tag object id) names, or None."""
    code, out, _ = git(["rev-parse", "--verify", "--quiet", "--end-of-options", rev + "^{commit}"], cwd)
    out = out.strip()
    return out if code == 0 and SHA_RE.fullmatch(out) else None


def remote_tag_commit(remote, tag, cwd=None):
    """The commit refs/tags/<tag> points at on the remote right now (peeled), or None if absent."""
    validate_tag(tag)
    ref = "refs/tags/" + tag
    code, out, err = git(["ls-remote", "--tags", "--end-of-options", remote, ref, ref + "^{}"], cwd)
    if code != 0:
        raise GuardError("git ls-remote %s failed: %s" % (remote, err.strip()[:300]))
    direct = peeled = None
    for line in out.splitlines():
        parts = line.split("\t")
        if len(parts) != 2:
            continue
        sha, name = parts
        if name == ref:
            direct = sha
        elif name == ref + "^{}":
            peeled = sha
    return peeled or direct


def on_main(commit, remote, cwd=None):
    """True if commit is reachable from the remote's main branch, fetched now into a private ref."""
    private = "refs/release-guard/main"
    code, _, err = git(["fetch", "--quiet", "--no-tags", "--end-of-options", remote,
                        "+refs/heads/main:" + private], cwd)
    if code != 0:
        raise GuardError("could not fetch main from %s: %s" % (remote, err.strip()[:300]))
    code, _, _ = git(["merge-base", "--is-ancestor", commit, private], cwd)
    return code == 0


def fetch_ci_runs(repo, sha, token, api="https://api.github.com"):
    """Workflow runs of ci.yml for this exact commit, from the REST API (any event; filtered later)."""
    validate_repo(repo)
    validate_sha(sha)
    query = urllib.parse.urlencode({"head_sha": sha, "per_page": "100"})
    st, body = GitHubAPI(token, api).call("GET", "/repos/%s/actions/workflows/%s/runs?%s"
                                          % (repo, CI_WORKFLOW_FILE, query))
    if st != 200 or not isinstance(body, dict):
        raise GuardError("listing ci.yml runs failed (HTTP %s)" % st)
    return body.get("workflow_runs", [])


def green_push_run(runs, sha):
    """A completed, successful ci.yml run started by a push of exactly this commit.

    A pull_request run does not count: it tests a merge of the branch into its base, not this
    commit. Fields are read from the run objects themselves, not trusted to the API's filters.
    """
    for r in runs:
        path = str(r.get("path") or "").split("@", 1)[0]
        if (r.get("event") == "push" and r.get("head_sha") == sha and path == CI_WORKFLOW_PATH
                and r.get("status") == "completed" and r.get("conclusion") == "success"):
            return r
    return None


# ---------------------------------------------------------------------------------------------
# meta


def tag_problems(tag, version, commit, remote, runs, cwd=None):
    """Every reason this tag must not become a draft release. An empty list means go."""
    problems = []
    if tag != "v" + version:
        problems.append("tag %s is not v + the workspace version %s in Cargo.toml; bump the version "
                        "on main first" % (tag, version))
    if not on_main(commit, remote, cwd):
        problems.append("commit %s is not on main" % commit)
    if green_push_run(runs, commit) is None:
        problems.append("no successful ci.yml run triggered by a push of %s (pull_request runs do "
                        "not count); push it, or re-run CI, first" % commit)
    at = remote_tag_commit(remote, tag, cwd)
    if at is None:
        problems.append("tag %s no longer exists on %s" % (tag, remote))
    elif at != commit:
        problems.append("tag %s now points at %s, not at %s that this run checked out; it was "
                        "moved" % (tag, at, commit))
    return problems


def meta(env, cwd=None, remote="origin", fetch_runs=None, out=sys.stdout):
    """Returns the outputs dict. Raises GuardError when this run must stop."""
    event = env.get("EVENT", "")
    ref_type = env.get("REF_TYPE", "")
    ref = env.get("GITHUB_REF", "")
    repo = validate_repo(env.get("GITHUB_REPOSITORY", ""))
    with open(os.path.join(cwd or ".", "Cargo.toml"), encoding="utf-8") as f:
        version = workspace_version(f.read())
    commit = peel_local(validate_sha(env.get("GITHUB_SHA", "")), cwd)
    if commit is None:
        raise GuardError("GITHUB_SHA does not name a commit in this checkout")
    if fetch_runs is None:
        token = env.get("GH_TOKEN", "")
        api = env.get("GITHUB_API_URL", "https://api.github.com")
        fetch_runs = lambda r, s: fetch_ci_runs(r, s, token, api)  # noqa: E731

    if event == "push" and ref_type == "tag":
        # The tag is checked before anything else looks at it.
        tag = validate_tag(env.get("REF_NAME", ""))
        if ref != "refs/tags/" + tag:
            raise GuardError("GITHUB_REF %r does not match the tag name" % (ref,))
        problems = tag_problems(tag, version, commit, remote, fetch_runs(repo, commit), cwd)
        if problems:
            raise GuardError("refusing to make a release: " + "; ".join(problems))
        publish = "true"
    else:
        if ref_type == "tag" or ref.startswith("refs/tags/"):
            raise GuardError("a rehearsal runs from a branch, not from a tag; only a tag push makes "
                             "a release")
        tag = rehearsal_tag(version, env.get("RUN_ID", ""))
        publish = "false"
        # Shows what the tag path would decide for this commit; never fails the rehearsal.
        try:
            would = tag_problems("v" + version, version, commit, remote, fetch_runs(repo, commit), cwd)
        except Exception as e:  # report only
            would = ["could not evaluate: %s" % e]
        print("rehearsal: a tag v%s on this commit would be %s" % (
            version, "refused: " + "; ".join(would) if would else "accepted"), file=out)
    return {"tag": tag, "version": version, "sha": commit, "publish": publish}


def write_outputs(outputs, path):
    for k, v in outputs.items():
        if "\n" in v or "\r" in v:
            raise GuardError("output %s contains a line break" % k)
    with open(path, "a", encoding="utf-8") as f:
        for k, v in outputs.items():
            f.write("%s=%s\n" % (k, v))


# ---------------------------------------------------------------------------------------------
# The published verification command. README.md, site/index.html, docs/RELEASING.md and the
# release notes print exactly this; the tests compare them with it.


def verify_args(file, repo, tag):
    return ["gh", "attestation", "verify", file,
            "--repo", repo,
            "--signer-workflow", repo + "/" + RELEASE_WORKFLOW,
            "--source-ref", "refs/tags/" + tag,
            "--deny-self-hosted-runners"]


def verify_command(file, repo, tag):
    return " ".join(verify_args(file, repo, tag))


def published_files(dist):
    with open(os.path.join(dist, SUMS), encoding="utf-8") as f:
        names = [line.split("  ", 1)[1].strip() for line in f if line.strip()]
    for n in names:
        if not NAME_RE.fullmatch(n):
            raise GuardError("unexpected file name %r in %s" % (n, SUMS))
    return names


def check_attestations(dist, repo, tag, publish, source_ref, run=None, sleep=time.sleep, tries=4,
                       out=sys.stdout):
    """Run the published command against every file in SHA256SUMS.

    For a release (publish true) each must pass. For a rehearsal each must FAIL, and the same flags
    with this run's own ref must pass, which shows the failure comes from the ref pin and not from a
    missing attestation or a wrong flag.
    """
    validate_tag(tag)
    validate_repo(repo)
    if run is None:
        def run(args):
            p = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)
            if p.returncode != 0:
                # Shows why gh refused (an expected source-ref mismatch or something else).
                print("  exit %d from %s:\n    %s" % (p.returncode, " ".join(args[3:]),
                                                      p.stdout.strip()[-1500:].replace("\n", "\n    ")), file=out)
            return p.returncode
    files = published_files(dist)
    if not files:
        raise GuardError("%s lists no files" % SUMS)

    def passes_eventually(args):
        for i in range(tries):
            if run(args) == 0:
                return True
            if i + 1 < tries:
                sleep(15)
        return False

    failures = []
    for name in files:
        path = os.path.join(dist, name)
        published = verify_args(path, repo, tag)
        if publish == "true":
            ok = passes_eventually(published)
            print("%s: published command %s" % (name, "passes" if ok else "FAILS"), file=out)
            if not ok:
                failures.append(name + ": the published command does not pass")
        else:
            if not source_ref.startswith("refs/heads/"):
                raise GuardError("a rehearsal must run from a branch ref, not %r" % (source_ref,))
            control = published[:]
            control[control.index("--source-ref") + 1] = source_ref
            ctl_ok = passes_eventually(control)
            pub_rc = run(published)
            print("%s: same flags with %s %s; published command (refs/tags/%s) %s" % (
                name, source_ref, "pass" if ctl_ok else "FAIL",
                tag, "is rejected" if pub_rc != 0 else "PASSES"), file=out)
            if not ctl_ok:
                failures.append(name + ": no attestation for this run's ref")
            if pub_rc == 0:
                failures.append(name + ": a rehearsal build passes the published release command")
    if failures:
        raise GuardError("; ".join(failures))


# ---------------------------------------------------------------------------------------------
# package


def write_sums(dist):
    names = sorted(n for n in os.listdir(dist) if n != SUMS)
    for n in names:
        p = os.path.join(dist, n)
        if os.path.islink(p) or not os.path.isfile(p):
            # e.g. a re-run that downloaded an earlier release-files artifact into dist/dist/
            raise GuardError("%s in %s is not a regular file; refusing to guess what to publish" % (n, dist))
    if not names:
        raise GuardError("nothing to publish in %s" % dist)
    lines = []
    for n in names:
        if not NAME_RE.fullmatch(n):
            raise GuardError("unexpected file name %r in %s" % (n, dist))
        h = hashlib.sha256()
        with open(os.path.join(dist, n), "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        lines.append("%s  %s\n" % (h.hexdigest(), n))
    with open(os.path.join(dist, SUMS), "w", encoding="utf-8", newline="\n") as f:
        f.writelines(lines)
    return names


def render_notes(template, values):
    text = template
    for key in PLACEHOLDERS:
        text = text.replace("@%s@" % key, values[key])
    left = sorted(set(re.findall(r"@[A-Z_]+@", text)))
    if left:
        raise GuardError("unfilled placeholder(s) in the release notes: %s" % ", ".join(left))
    return text


def package(dist, template_path, notes_out, tag, version, sha, bin_name, repo):
    validate_tag(tag)
    validate_version(version)
    validate_sha(sha)
    validate_repo(repo)
    if not NAME_RE.fullmatch(bin_name or ""):
        raise GuardError("binary name %r" % (bin_name,))
    names = write_sums(dist)
    with open(template_path, encoding="utf-8") as f:
        notes = render_notes(f.read(), {"BIN": bin_name, "TAG": tag, "VERSION": version,
                                        "REPO": repo, "SHA": sha})
    with open(notes_out, "w", encoding="utf-8") as f:
        f.write(notes)
    return names


# ---------------------------------------------------------------------------------------------
# check-binding


def manual_cleanup(repo, identity):
    """What a maintainer does with a draft this run made and left in place. Nothing here removes it."""
    rid = identity.get("id")
    if not isinstance(rid, int):
        return ("a draft this run created could not be identified here (its identity record is missing or "
                "unreadable); it was left in place and nothing was deleted. Do not publish it. List the "
                "releases for the tag (docs/RELEASING.md, step 4), leave any published release alone, and "
                "remove the draft by hand (docs/RELEASING.md, \"A draft left by a failed run\")")
    return ("release %s (%s) is a draft this run created; it was left in place and nothing was deleted. "
            "Do not publish it. To remove it by hand: check that `gh api repos/%s/releases/%s --jq .draft` "
            "prints true, then delete that release by its id (docs/RELEASING.md, \"A draft left by a failed "
            "run\")" % (rid, identity.get("html_url") or "no URL recorded", repo, rid))


def report(summary, text):
    """Also write a failure to the job summary page, when the workflow provides one."""
    if summary:
        try:
            with open(summary, "a", encoding="utf-8") as f:
                f.write("### Release guard\n\n%s\n\n" % text)
        except OSError as e:  # never let the summary replace the failure being reported
            print("could not write the job summary (%s)" % e, file=sys.stderr)


@contextlib.contextmanager
def draft_retained(repo, identity, summary):
    """Wraps everything that runs once a draft exists. Any failure inside, of any kind (a guard
    refusal, a git or network error, a bug, an interrupt), is reported the same way: its own reason,
    then the retained draft's id, URL and the manual steps, in the error and in the job summary."""
    try:
        yield
    except BaseException as e:
        reason = str(e) if isinstance(e, GuardError) else "%s: %s" % (type(e).__name__, e)
        why = "%s; %s" % (reason, manual_cleanup(repo, identity))
        report(summary, why)
        if isinstance(e, Exception):
            raise GuardError(why) from e
        raise  # KeyboardInterrupt, SystemExit (a cancelled job): reported, then re-raised as is


def load_identity(path):
    try:
        with open(path, encoding="utf-8") as f:
            identity = json.load(f)
        return identity if isinstance(identity, dict) else {}
    except (OSError, ValueError):
        return {}


def require_binding(tag, sha, remote, cwd, out):
    at = remote_tag_commit(remote, tag, cwd)
    if at != sha:
        raise GuardError("tag %s now points at %s, not at the built commit %s" % (tag, at or "nothing", sha))
    print("tag %s points at %s, the commit that was built" % (tag, sha), file=out)


def check_binding(tag, sha, remote="origin", cwd=None, after_create=False, repo=None,
                  identity_path=IDENTITY_FILE, out=sys.stdout, summary=None):
    """The tag on the remote still points at the built commit. With after_create (a draft exists),
    every failure names the draft this run created and how to remove it by hand; the guard never
    deletes a release."""
    validate_tag(tag)
    validate_sha(sha)
    if after_create:
        with draft_retained(validate_repo(repo or ""), load_identity(identity_path), summary):
            require_binding(tag, sha, remote, cwd, out)
        return
    try:
        require_binding(tag, sha, remote, cwd, out)
    except GuardError as e:
        report(summary, str(e))
        raise


# ---------------------------------------------------------------------------------------------
# create-draft. One HTTP client, one place the token is attached, GET and POST only: nothing in
# this script or the workflow edits, publishes or deletes a release. A draft that a failed run
# leaves behind stays until a maintainer removes it by hand.

IDENTITY_KEYS = ("id", "node_id", "tag_name", "created_at", "author", "html_url")
RELEASE_TITLE = "PruneDisk %s"
ALLOWED_METHODS = ("GET", "POST")


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Never follow a redirect. The request carries the token, and urllib's default handler resends
    it, Authorization header included, to wherever Location points. None of the endpoints used here
    redirect in normal operation (a renamed repository answers 301; the workflow uses the current
    name), so a redirect is an error."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise GuardError("HTTP %d redirect to %s refused; nothing is sent after a redirect"
                         % (code, urllib.parse.urlparse(newurl).hostname))


class GitHubAPI:
    """Minimal REST client: HTTPS only, the token only to the API host and GitHub's upload host
    (checked on every request), no redirects followed, no proxy taken from the environment."""

    def __init__(self, token, api="https://api.github.com", transport=None):
        self.api = api.rstrip("/")
        parsed = urllib.parse.urlparse(self.api)
        if parsed.scheme != "https" or not parsed.hostname:
            raise GuardError("API URL %r is not https" % (api,))
        self.hosts = {parsed.hostname}
        if parsed.hostname == "api.github.com":
            self.hosts.add("uploads.github.com")
        self.token = token
        self.transport = transport or self._urllib

    def _urllib(self, method, url, data, headers):
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect())
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with opener.open(req, timeout=120) as resp:
                return resp.status, resp.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    def call(self, method, url, body=None, raw=None, content_type="application/json"):
        if method not in ALLOWED_METHODS:
            raise GuardError("HTTP %s refused: the release guard never edits, publishes or deletes a "
                             "release" % method)
        if "://" not in url:
            url = self.api + url
        target = urllib.parse.urlparse(url)
        if target.scheme != "https" or target.hostname not in self.hosts:
            raise GuardError("refusing to send the token to %s" % target.hostname)
        data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
        headers = {"Accept": "application/vnd.github+json", "Authorization": "Bearer " + self.token,
                   "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "release-guard"}
        if data is not None:
            headers["Content-Type"] = content_type
        status, payload = self.transport(method, url, data, headers)
        try:
            parsed = json.loads(payload.decode() or "null") if payload else None
        except ValueError:
            parsed = None
        return status, parsed


def identity_of(release):
    return {"id": release.get("id"), "node_id": release.get("node_id"), "tag_name": release.get("tag_name"),
            "created_at": release.get("created_at"),
            "author": release["author"].get("login") if isinstance(release.get("author"), dict) else None,
            "html_url": release.get("html_url")}


def release_files(dist):
    names = sorted(os.listdir(dist))
    for n in names:
        p = os.path.join(dist, n)
        if not NAME_RE.fullmatch(n) or os.path.islink(p) or not os.path.isfile(p):
            raise GuardError("%r in %s is not a plain file to publish" % (n, dist))
    if SUMS not in names or len(names) < 2:
        raise GuardError("%s must hold %s and the archives" % (dist, SUMS))
    return names


def check_sums(dist, names):
    """Every file to upload is listed in SHA256SUMS with its hash, and nothing else is listed."""
    listed = {}
    with open(os.path.join(dist, SUMS), encoding="utf-8") as f:
        for line in f.read().splitlines():
            h, sep, n = line.partition("  ")
            if not sep or not re.fullmatch(r"[0-9a-f]{64}", h) or n in listed:
                raise GuardError("%s has a malformed line %r" % (SUMS, line[:100]))
            listed[n] = h
    files = [n for n in names if n != SUMS]
    if sorted(listed) != files:
        raise GuardError("%s lists %s but dist holds %s" % (SUMS, sorted(listed), files))
    for n in files:
        h = hashlib.sha256()
        with open(os.path.join(dist, n), "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        if h.hexdigest() != listed[n]:
            raise GuardError("%s does not match its line in %s; it changed after packaging" % (n, SUMS))


def releases_for_tag(api, repo, tag, max_pages=20):
    """Every release whose tag is this tag, drafts included (the job's token can see them)."""
    found = []
    for page in range(1, max_pages + 1):
        st, rels = api.call("GET", "/repos/%s/releases?%s" % (repo, urllib.parse.urlencode(
            {"per_page": "100", "page": str(page)})))
        if st != 200 or not isinstance(rels, list):
            raise GuardError("listing releases failed (HTTP %s)" % st)
        found += [r for r in rels if isinstance(r, dict) and r.get("tag_name") == tag]
        if len(rels) < 100:
            return found
    raise GuardError("more than %d pages of releases; not guessing whether %s already has one" % (max_pages, tag))


def create_draft(api, repo, tag, dist, notes_path, identity_path=IDENTITY_FILE, out=sys.stdout, summary=None):
    """Create a draft release, upload every file in dist (SHA256SUMS last), and record its identity.

    Refuses when any release already carries the tag (for example a re-run of a failed run). On a
    failure after the draft exists, leaves it in place, names it and fails. Returns the identity.
    """
    validate_tag(tag)
    validate_repo(repo)
    names = release_files(dist)
    check_sums(dist, names)
    with open(notes_path, encoding="utf-8") as f:
        notes = f.read()
    existing = releases_for_tag(api, repo, tag)
    if existing:
        why = ("a release for %s already exists (%s); this run does not create a second one. If it is a "
               "draft a failed run left, remove it by hand (docs/RELEASING.md, \"A draft left by a failed "
               "run\") and re-run" % (tag, ", ".join(
                   "id %s, %s" % (r.get("id"), "draft" if r.get("draft") else "published") for r in existing)))
        report(summary, why)
        raise GuardError(why)
    try:
        status, rel = api.call("POST", "/repos/%s/releases" % repo, {
            "tag_name": tag, "name": RELEASE_TITLE % tag, "body": notes, "draft": True,
            "prerelease": is_prerelease(tag)})
        if status != 201 or not isinstance(rel, dict) or not isinstance(rel.get("id"), int):
            raise GuardError("creating the draft failed (HTTP %s): %s" % (status, str(rel)[:300]))
    except BaseException as e:
        # The request may have reached GitHub even if no answer came back (a lost response).
        why = ("%s; a draft for %s may exist anyway: list the releases for the tag (docs/RELEASING.md, "
               "step 4) before re-running; a re-run refuses while one exists"
               % (e if isinstance(e, GuardError) else "%s: %s" % (type(e).__name__, e), tag))
        report(summary, why)
        if isinstance(e, Exception):
            raise GuardError(why) from e
        raise
    identity = identity_of(rel)
    # From here on a draft exists: every failure, whatever it is, goes through draft_retained.
    with draft_retained(repo, identity, summary):
        with open(identity_path, "w", encoding="utf-8") as f:
            json.dump(identity, f)
        if rel.get("draft") is not True or rel.get("tag_name") != tag:
            raise GuardError("GitHub did not create a draft for %s" % tag)
        upload = str(rel.get("upload_url") or "").split("{", 1)[0]
        # SHA256SUMS goes last: a draft without it is incomplete (docs/RELEASING.md, step 4).
        for n in [x for x in names if x != SUMS] + [SUMS]:
            with open(os.path.join(dist, n), "rb") as f:
                data = f.read()
            st, asset = api.call("POST", upload + "?" + urllib.parse.urlencode({"name": n}), raw=data,
                                 content_type="application/octet-stream")
            if st != 201 or not isinstance(asset, dict) or asset.get("name") != n or asset.get("size") != len(data):
                raise GuardError("uploading %s failed (HTTP %s)" % (n, st))
        st, now = api.call("GET", "/repos/%s/releases/%d" % (repo, identity["id"]))
        now = now if isinstance(now, dict) else {}
        got = sorted(str(a.get("name")) for a in now.get("assets", []))
        if st != 200 or now.get("draft") is not True or got != names:
            raise GuardError("the draft does not hold exactly the files built (HTTP %s, assets %s)" % (st, got))
        print("draft=true release_id=%d assets=%d (%s)" % (identity["id"], len(names), ", ".join(names)), file=out)
    return identity


# ---------------------------------------------------------------------------------------------
# lint-workflow: what the release workflow must look like for the guards above to mean anything.

GUARD_LINES = {
    "meta": ["python3 -I -m unittest discover -s .github/scripts -p 'test_*.py'",
             "python3 -I .github/scripts/release_meta.py meta"],
    "package": ["python3 -I .github/scripts/release_meta.py package",
                "python3 -I .github/scripts/release_meta.py check-attestations"],
    "draft-release": ["python3 -I .github/scripts/release_meta.py check-binding",
                      "python3 -I .github/scripts/release_meta.py create-draft",
                      "python3 -I .github/scripts/release_meta.py check-binding --after-create"],
}
# Releases are created and removed only by release_meta.py (draft only, removal by identity).
RELEASE_WRITES = re.compile(r"\bgh\s+(release|api)\b|\bcurl\b[^\n]*/releases")
GUARD_STEP_KEYS = {"name", "id", "env", "run"}
GATED_JOBS = ("meta", "build", "package", "draft-release")
NEEDS = {"build": "meta", "package": "[meta, build]", "draft-release": "[meta, package]"}


def split_jobs(text):
    m = re.search(r"^jobs:\s*$", text, re.M)
    if not m:
        return {}
    body = text[m.end():]
    heads = list(re.finditer(r"^  ([A-Za-z][\w-]*):\s*$", body, re.M))
    jobs = {}
    for i, h in enumerate(heads):
        end = heads[i + 1].start() if i + 1 < len(heads) else len(body)
        jobs[h.group(1)] = body[h.end():end]
    return jobs


def split_steps(job_text):
    starts = [m.start() for m in re.finditer(r"^      - ", job_text, re.M)]
    return [job_text[s:(starts[i + 1] if i + 1 < len(starts) else len(job_text))]
            for i, s in enumerate(starts)]


def run_blocks(text):
    """Every run: script in the file, block (run: |) or single line, as (line number, script)."""
    lines = text.splitlines()
    blocks = []
    i = 0
    while i < len(lines):
        m = re.match(r"^(\s*)(?:- )?run:\s*(.*)$", lines[i])
        if not m:
            i += 1
            continue
        indent = len(m.group(1))
        rest = m.group(2).strip()
        if rest in ("|", "|-", "|+", ">", ">-"):
            body = []
            j = i + 1
            while j < len(lines) and (not lines[j].strip() or len(lines[j]) - len(lines[j].lstrip()) > indent):
                body.append(lines[j])
                j += 1
            pad = min((len(l) - len(l.lstrip()) for l in body if l.strip()), default=0)
            blocks.append((i + 1, "\n".join(l[pad:] for l in body).strip("\n")))
            i = j
        else:
            blocks.append((i + 1, rest))
            i += 1
    return blocks


UNTRUSTED_CONTEXT = re.compile(
    r"\$\{\{[^}]*\b(github\.(event|head_ref|ref|ref_name|base_ref)|inputs\.|needs\.|steps\.)", re.I)
NESTED_SINKS = [
    (re.compile(r"-Command\s+\"[^\"\n]*\$"), "a shell variable inside a PowerShell -Command string"),
    (re.compile(r"\bsed\b[^\n]*\"[^\"\n]*\$"), "a shell variable inside a sed program"),
    (re.compile(r"\beval\b"), "eval"),
    (re.compile(r"\b(?:ba)?sh\s+-c\b"), "sh -c"),
]


def lint_workflow(text, strict=True):
    """Problems with a workflow file. strict (release.yml): no ${{ }} in any run script at all."""
    problems = []
    blocks = run_blocks(text)
    if not blocks:
        problems.append("no run: scripts found (the parser does not understand this file)")
    for line, script in blocks:
        if strict and "${{" in script:
            problems.append("line %d: ${{ }} inside a run script; pass it through env" % line)
        elif UNTRUSTED_CONTEXT.search(script):
            problems.append("line %d: untrusted context interpolated into a run script" % line)
        for rx, what in NESTED_SINKS:
            if rx.search(script):
                problems.append("line %d: %s" % (line, what))
        if strict and RELEASE_WRITES.search(script):
            problems.append("line %d: release created, changed or deleted outside release_meta.py" % line)
    if not strict:
        return problems

    if not re.search(r"^defaults:\s*\n\s+run:\s*\n\s+shell:\s*bash\s*$", text, re.M):
        problems.append("defaults.run.shell is not bash")
    jobs = split_jobs(text)
    for name in GATED_JOBS:
        if name not in jobs:
            problems.append("job %s is missing" % name)
            continue
        job = jobs[name]
        if re.search(r"^\s+continue-on-error:", job, re.M):
            problems.append("job %s has continue-on-error" % name)
        if re.search(r"^\s+if:.*\b(always|failure|cancelled)\s*\(", job, re.M):
            problems.append("job %s runs a step or itself after a failure" % name)
        if name in NEEDS and not re.search(r"^    needs: %s\s*$" % re.escape(NEEDS[name]), job, re.M):
            problems.append("job %s does not need %s" % (name, NEEDS[name]))
        scripts = []
        for step in split_steps(job):
            if "actions/checkout@" in step and name != "meta":
                if not re.search(r"^\s+ref: \$\{\{ needs\.meta\.outputs\.sha \}\}\s*$", step, re.M):
                    problems.append("job %s checks out something other than the commit meta checked" % name)
            step_scripts = [s for _, s in run_blocks(step)]
            if any(s in GUARD_LINES.get(name, []) for s in step_scripts):
                # A guard step may not be skipped, softened or moved: no if, continue-on-error,
                # shell or working-directory.
                keys = set(re.findall(r"^      - ([A-Za-z][\w-]*):", step, re.M))
                keys |= set(re.findall(r"^        ([A-Za-z][\w-]*):", step, re.M))
                extra = sorted(keys - GUARD_STEP_KEYS)
                if extra:
                    problems.append("job %s: guard step has %s" % (name, ", ".join(extra)))
            scripts.extend(step_scripts)
        for want in GUARD_LINES.get(name, []):
            if want not in scripts:
                problems.append("job %s: no step whose whole script is %r" % (name, want))
    draft = jobs.get("draft-release", "")
    if not re.search(r"^    if: needs\.meta\.outputs\.publish == 'true'\s*$", draft, re.M):
        problems.append("draft-release is not limited to publish == 'true'")
    if draft:
        # The write token reaches one step only: the one that creates the draft.
        for step in split_steps(draft):
            if re.search(r"^\s+GH_TOKEN:", step, re.M) and \
                    [s for _, s in run_blocks(step)] != [GUARD_LINES["draft-release"][1]]:
                problems.append("draft-release: a step other than create-draft is given GH_TOKEN")
        order = [s for _, s in run_blocks(draft)]
        try:
            before, create, after = (order.index(g) for g in GUARD_LINES["draft-release"])
            if not before < create < after:
                problems.append("draft-release: binding checks are not before and after the create")
        except ValueError:
            problems.append("draft-release: binding checks or create step missing")
    if len(re.findall(r"^\s+contents: write\s*$", text, re.M)) != 1 or "contents: write" not in draft:
        problems.append("contents: write must appear once, in draft-release")
    pkg = jobs.get("package", "")
    if not re.search(r"^\s+subject-checksums: dist/SHA256SUMS\s*$", pkg, re.M):
        problems.append("package does not attest every file listed in SHA256SUMS")
    return problems


# ---------------------------------------------------------------------------------------------


def _terminated(signum, frame):
    raise SystemExit(128 + signum)


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="cmd")
    sub.add_parser("meta")
    pk = sub.add_parser("package")
    pk.add_argument("--dist", default="dist")
    pk.add_argument("--notes", default=".github/release-notes.md")
    pk.add_argument("--notes-out", default="dist-notes.md")
    ca = sub.add_parser("check-attestations")
    ca.add_argument("--dist", default="dist")
    cb = sub.add_parser("check-binding")
    cb.add_argument("--after-create", action="store_true")
    cd = sub.add_parser("create-draft")
    cd.add_argument("--dist", default="out/dist")
    cd.add_argument("--notes", default="out/dist-notes.md")
    vt = sub.add_parser("validate-tag")
    vt.add_argument("tag")
    lw = sub.add_parser("lint-workflow")
    lw.add_argument("path", nargs="?", default=RELEASE_WORKFLOW)
    a = p.parse_args(argv)
    env = os.environ
    # A cancelled job gets SIGINT (KeyboardInterrupt) and then SIGTERM. Turn SIGTERM into SystemExit
    # so draft_retained can still name the draft before the process ends.
    signal.signal(signal.SIGTERM, _terminated)
    try:
        if a.cmd == "meta":
            outputs = meta(env)
            write_outputs(outputs, env["GITHUB_OUTPUT"])
            print(" ".join("%s=%s" % kv for kv in outputs.items()))
        elif a.cmd == "package":
            names = package(a.dist, a.notes, a.notes_out, env.get("TAG", ""), env.get("VERSION", ""),
                            env.get("SHA", ""), env.get("BIN_NAME", ""), env.get("GITHUB_REPOSITORY", ""))
            with open(os.path.join(a.dist, SUMS), encoding="utf-8") as f:
                sys.stdout.write(f.read())
            print("%d file(s) to publish" % len(names))
        elif a.cmd == "check-attestations":
            check_attestations(a.dist, env.get("GITHUB_REPOSITORY", ""), env.get("TAG", ""),
                               env.get("PUBLISH", ""), env.get("GITHUB_REF", ""))
        elif a.cmd == "check-binding":
            check_binding(env.get("TAG", ""), env.get("SHA", ""), after_create=a.after_create,
                          repo=env.get("GITHUB_REPOSITORY", ""), summary=env.get("GITHUB_STEP_SUMMARY"))
        elif a.cmd == "create-draft":
            api = GitHubAPI(env.get("GH_TOKEN", ""), env.get("GITHUB_API_URL", "https://api.github.com"))
            create_draft(api, env.get("GITHUB_REPOSITORY", ""), env.get("TAG", ""), a.dist, a.notes,
                         summary=env.get("GITHUB_STEP_SUMMARY"))
        elif a.cmd == "validate-tag":
            validate_tag(a.tag)
        elif a.cmd == "lint-workflow":
            with open(a.path, encoding="utf-8") as f:
                problems = lint_workflow(f.read())
            for pr in problems:
                print(pr)
            if problems:
                return 1
        else:
            p.print_help()
            return 2
    except GuardError as e:
        print("::error::%s" % e)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
