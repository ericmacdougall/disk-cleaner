#!/usr/bin/env python3
"""Release guards for .github/workflows/release.yml, in one place so they can be tested.

Every value that comes from a ref name (the tag) passes validate_tag() before anything uses it, and
reaches git and gh only as a separate argument (never through a shell). The workflow calls this
script; it never builds a command line out of the tag itself.

Subcommands (all read their inputs from the environment the workflow sets, see each function):

  meta               decide tag, version, commit and whether this run may create a draft release
  package            write dist/SHA256SUMS and fill in the release notes
  check-attestations run the published verification command against every file in SHA256SUMS
  check-binding      the tag on the remote still points at the commit that was built
  lint-workflow      static checks over release.yml (also run by the tests)

These guards are hygiene. They run from the workflow file in the tagged commit, so someone who can
push a tag can also push a commit whose release.yml leaves them out. The real controls are the
repository settings and the maintainer's publish step described in docs/RELEASING.md.

Python 3.9 compatible, standard library only.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import time
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
    url = "%s/repos/%s/actions/workflows/%s/runs?%s" % (api.rstrip("/"), repo, CI_WORKFLOW_FILE, query)
    req = urllib.request.Request(url, headers={
        "Accept": "application/vnd.github+json",
        "Authorization": "Bearer " + token,
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "release-guard",
    })
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp).get("workflow_runs", [])


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


def check_binding(tag, sha, remote="origin", cwd=None, after_create=False, run=None, repo=None,
                  out=sys.stdout):
    validate_tag(tag)
    validate_sha(sha)
    at = remote_tag_commit(remote, tag, cwd)
    if at == sha:
        print("tag %s points at %s, the commit that was built" % (tag, sha), file=out)
        return
    why = "tag %s now points at %s, not at the built commit %s" % (tag, at or "nothing", sha)
    if after_create:
        # The draft was made for a tag that moved: remove that draft (only a draft) and fail.
        if run is None:
            run = lambda args: subprocess.run(args).returncode  # noqa: E731
        repo = validate_repo(repo or "")
        rc = run(["gh", "release", "delete", tag, "--repo", repo, "--yes"])
        why += "; the draft just created was %s" % ("deleted" if rc == 0 else "NOT deleted, delete it by hand")
    raise GuardError(why)


# ---------------------------------------------------------------------------------------------
# lint-workflow: what the release workflow must look like for the guards above to mean anything.

GUARD_LINES = {
    "meta": ["python3 -I -m unittest discover -s .github/scripts -p 'test_*.py'",
             "python3 -I .github/scripts/release_meta.py meta"],
    "package": ["python3 -I .github/scripts/release_meta.py package",
                "python3 -I .github/scripts/release_meta.py check-attestations"],
    "draft-release": ["python3 -I .github/scripts/release_meta.py check-binding",
                      "python3 -I .github/scripts/release_meta.py check-binding --after-create"],
}
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
        order = [s for _, s in run_blocks(draft)]
        try:
            before = order.index(GUARD_LINES["draft-release"][0])
            create = next(i for i, s in enumerate(order) if "gh release create" in s)
            after = order.index(GUARD_LINES["draft-release"][1])
            if not before < create < after:
                problems.append("draft-release: binding checks are not before and after the create")
        except (ValueError, StopIteration):
            problems.append("draft-release: binding checks or create step missing")
        if "--draft" not in draft:
            problems.append("draft-release does not pass --draft")
    if len(re.findall(r"^\s+contents: write\s*$", text, re.M)) != 1 or "contents: write" not in draft:
        problems.append("contents: write must appear once, in draft-release")
    pkg = jobs.get("package", "")
    if not re.search(r"^\s+subject-checksums: dist/SHA256SUMS\s*$", pkg, re.M):
        problems.append("package does not attest every file listed in SHA256SUMS")
    return problems


# ---------------------------------------------------------------------------------------------


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
    vt = sub.add_parser("validate-tag")
    vt.add_argument("tag")
    lw = sub.add_parser("lint-workflow")
    lw.add_argument("path", nargs="?", default=RELEASE_WORKFLOW)
    a = p.parse_args(argv)
    env = os.environ
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
                          repo=env.get("GITHUB_REPOSITORY", ""))
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
