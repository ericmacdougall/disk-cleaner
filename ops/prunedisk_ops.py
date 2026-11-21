#!/usr/bin/env python3
"""prunedisk-ops: apply-side companion to `disk-cleaner scan/plan`.

Every step is a DRY RUN unless --apply is given. Order: inventory -> plan -> recover -> backup -> manifests -> wipe.
  inventory   read-only: units (top-level work folders), sizes, git facts (fresh ls-remote), live/service paths,
              log/transcript files, media files. Aborts if a liveness probe fails.
  plan        keep / secure-then-wipe per unit, with the reasons.
  recover     push local-only commits and a snapshot of uncommitted work to NEW branches
              recovery/<host>-<date>-<desc> on PRIVATE origins (never force, never an existing branch, no history
              rewrite). Pushes are secret-scanned and size-capped; anything not pushed is archived whole instead.
  backup      archives (tar+zstd, written locally, `zstd -t` checked, uploaded with rclone, storage SHA-1 compared with
              the local SHA-1) of: logs/transcripts, media not recoverable from git, and every wipe unit's residue
              (everything not provably recoverable: unsecured repos whole, .git metadata, untracked/modified files).
              Secret-named files go to a local 0700 vault, never to storage. Each archive gets a full path index.
  manifests   per-origin manifest (private repos only) + a full index in `index_repo`, on a `backup-manifests` branch.
  wipe        for each unit still idle, unused, unreferenced, unchanged since its residue backup (ctime), with no mount
              inside and no worktree/alternates user elsewhere: rename into quarantine, then delete. Every deletion logged.
Config: JSON (see ops/README.md). State: --state (default ~/.prunedisk-ops/<date>)."""
import argparse, fnmatch, glob, hashlib, json, os, re, shutil, socket, stat, subprocess, sys, tarfile, tempfile, time

DEFAULTS = {
    "home": os.path.expanduser("~"), "host": socket.gethostname().split(".")[0], "date": time.strftime("%Y%m%d"),
    "remote": "", "active_hours": 24, "protect_names": [], "keep": [], "keep_globs": [],
    "containers": [".agent-harness/work", "repos", "work", "wt"],
    "tool_dirs": ["bin", "opt", "android-sdk", "sdk", "toolchains", "archive", "go", "snap", "Desktop", "Documents",
                  "Downloads", "Pictures", "Music", "Videos", "Templates", "Public", "__pycache__", "Library",
                  "Applications", "Movies", "Dropbox", "Google Drive", "OneDrive", "Nextcloud", "Sync", "pCloudDrive",
                  "miniconda3", "anaconda3", "Android", "google-cloud-sdk", "flutter", "node", "go-sdk"],
    # unambiguous rebuildable folders; one only counts when git tracks nothing in it and it holds no repo.
    # value: marker beside it ("" none, "INSIDE:x" = x inside it)
    "regenerable": {"node_modules": "", "__pycache__": "", ".pytest_cache": "", ".mypy_cache": "", ".ruff_cache": "",
                    ".tox": "", ".turbo": "", ".parcel-cache": "", ".nx": "", ".next": "package.json",
                    ".nuxt": "package.json", ".svelte-kit": "package.json", ".venv": "INSIDE:pyvenv.cfg",
                    "venv": "INSIDE:pyvenv.cfg", "target": "Cargo.toml"},
    "salvage_repo": "", "index_repo": "", "max_residue_gb": 5.0, "max_push_mb": 300, "max_branches_per_repo": 6,
    "max_backup_age_hours": 12, "credential_helper": "", "allow_file_remotes": False, "visibility_default": "PUBLIC",
    "log_dirs": [".claude/projects", ".claude/todos", ".claude/history.jsonl", ".codex/sessions", ".codex/archived_sessions",
                 ".codex/history.jsonl", ".agent-harness/logs", ".agent-harness/codex-wals", ".local/share/claude"],
    "file_scan_prune": [".cache", ".local/share/pnpm", ".npm", ".gradle", ".m2", ".rustup", ".cargo", ".android", "opt",
                        "android-sdk", "sdk", "toolchains", ".prunedisk-ops", ".disk-cleaner", "go", "snap"],
}
LOG_RE = re.compile(r"(\.log(\.\d+)?(\.gz|\.zst)?|\.jsonl(\.gz|\.zst)?|\.wal|\.cast|transcript[^/]*)$", re.I)
MEDIA_RE = re.compile(r"\.(mp4|mov|webm|mkv|avi|wav|mp3|m4a|flac|ogg|aac|png|jpe?g|gif|webp|pdf|pptx|psd)$", re.I)
SECRET_NAME = re.compile(r"^(\.env(\..+)?|\.envrc|\.dev\.vars|.+\.pem|.+\.key|id_(rsa|ed25519|ecdsa|dsa)|credentials(\.json)?|\.npmrc|"
                         r"\.netrc|\.git-credentials|\.pgpass|\.pypirc|kubeconfig|.+\.ovpn|.+\.tfstate(\..*)?|.+\.tfvars|"
                         r".+\.p12|.+\.pfx|.+\.keystore|.+\.jks|service-account.*\.json|.*secret.*\.(json|ya?ml|txt)|"
                         r".*private[-_.]?key.*|.*token.*\.(json|txt)|\.htpasswd|.+\.kdbx|.+\.gpg|.+\.asc|.*secret.*\.(toml|ini|env|conf)|"
                         r"secrets?\..*)$", re.I)
OPAQUE_RE = re.compile(r"\.(zip|tar|tgz|gz|bz2|xz|zst|7z|rar|jar|war|docx|xlsx|pptx|odt|ods|sqlite3?|db|dump|bak|pkl|npz|h5)$", re.I)
SECRET_OK = re.compile(r"\.(example|sample|template|dist|defaults?)$|\.pub$", re.I)
SECRET_CONTENT = re.compile(rb"(AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,}|sk-[A-Za-z0-9_-]{32,}|"
                            rb"xox[abpr]-[A-Za-z0-9-]{10,}|-----BEGIN [A-Z ]*PRIVATE KEY-----|AIza[0-9A-Za-z_-]{35}|"
                            rb"sk_live_[0-9a-zA-Z]{20,}|rk_live_[0-9a-zA-Z]{20,}|glpat-[A-Za-z0-9_-]{20,}|npm_[A-Za-z0-9]{36}|hf_[A-Za-z0-9]{30,}|"
                            rb"xoxe-[A-Za-z0-9-]{10,}|SG\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}|ya29\.[A-Za-z0-9_-]{20,}|eyJhbGciOi[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}|"
                            rb"://[^/\s:@]{1,64}:[^/\s@]{6,}@)")
NOISE = re.compile(r"(^|/)(node_modules|__pycache__|\.pytest_cache|\.venv|\.turbo|\.next)(/|$)|(\.pyc|\.tsbuildinfo)$")
GENV = dict(os.environ, GIT_OPTIONAL_LOCKS="0", GIT_TERMINAL_PROMPT="0", GIT_SSH_COMMAND="ssh -o BatchMode=yes -o ConnectTimeout=15")
GSAFE = ["-c", "core.fsmonitor=false", "-c", "gc.auto=0", "-c", "maintenance.auto=false", "-c", "core.hooksPath=/dev/null"]
NET = re.compile(r"^(https?://|ssh://|git@)")
C = {}
NOW = time.time()


class ProbeError(Exception):
    pass


def log(*a):
    print(*a, file=sys.stderr, flush=True)


# Local-only git commands must never lazily fetch missing objects from a promisor remote (partial clones made
# with --filter=blob:none): cat-file/rev-list on an advertised sha would download it, one object at a time.
# git 2.43 has no GIT_NO_LAZY_FETCH, so allow no transport protocol at all for local commands.
GENV_LOCAL = dict(GENV, GIT_NO_LAZY_FETCH="1", GIT_ALLOW_PROTOCOL="none-allowed")
NET_CMDS = {"ls-remote", "fetch", "push", "clone", "remote"}


def git(path, *args, t=120, env=None, inp=None):
    if env is None:
        env = GENV if (args and args[0] in NET_CMDS) else GENV_LOCAL
    try:
        r = subprocess.run(["git", "-C", path] + GSAFE + list(args), capture_output=True, timeout=t, env=env, input=inp)
        return r.returncode, r.stdout.decode("utf-8", "replace"), r.stderr.decode("utf-8", "replace")
    except Exception as e:
        return -1, "", str(e)


_VIEWS = {}


def objview(path):
    """Partial clones (promisor remotes) lazily fetch any object they lack; with the network blocked, git then spins
    for minutes per missing object. For presence checks and walks, return a throwaway bare repo that borrows this
    repo's objects through alternates and has no promisor remote, so a missing object is simply reported missing.
    Returns (dir, is_view). The original repo is never modified."""
    rc, o, _ = git(path, "config", "--get-regexp", r"^(extensions\.partialclone|remote\..*\.promisor)$")
    if rc != 0 or not o.strip():
        return path, False
    rc, cd, _ = git(path, "rev-parse", "--path-format=absolute", "--git-common-dir")
    if rc != 0 or not cd.strip():
        return path, False
    objs = os.path.join(os.path.realpath(cd.strip()), "objects")
    if objs in _VIEWS:
        return _VIEWS[objs], True
    base = os.path.join(C.get("state") or tempfile.gettempdir(), "tmp")
    os.makedirs(base, exist_ok=True)
    d = tempfile.mkdtemp(prefix="objview-", dir=base)
    if subprocess.run(["git", "init", "-q", "--bare", d], capture_output=True, env=GENV_LOCAL).returncode != 0:
        shutil.rmtree(d, ignore_errors=True)
        return path, False
    with open(os.path.join(d, "objects", "info", "alternates"), "w") as f:
        f.write(objs + "\n")
    _VIEWS[objs] = d
    return d, True


def _drop_views():
    for d in _VIEWS.values():
        shutil.rmtree(d, ignore_errors=True)


import atexit
atexit.register(_drop_views)


def run(cmd, t=120):
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=t, errors="replace")
        return r.returncode, r.stdout, r.stderr
    except FileNotFoundError:
        return 127, "", "not installed"
    except Exception as e:
        return -1, "", str(e)


def rel(p):
    h = C["home"]
    return p[len(h) + 1:] if p.startswith(h + "/") else p


def slug(s, n=60):
    s = re.sub(r"[^A-Za-z0-9._-]+", "-", s).strip("-.")
    return s[:n].strip("-.") or "x"


def keyname(p):
    """storage-safe unique name for a path: readable slug + hash of the full path (no collisions)."""
    return "%s-%s" % (slug(rel(p).replace("/", "__"), 100), hashlib.sha1(p.encode()).hexdigest()[:8])


def clean_url(u):
    return re.sub(r"://[^/@\s]+@", "://", u or "")


def jdump(obj, name):
    p = os.path.join(C["state"], name)
    with open(p + ".tmp", "w") as f:
        json.dump(obj, f, indent=1, sort_keys=True)
    os.replace(p + ".tmp", p)
    return p


def jload(name, default=None):
    p = os.path.join(C["state"], name)
    if not os.path.exists(p):
        if default is not None:
            return default
        sys.exit("missing %s: run the earlier step first" % p)
    with open(p) as f:
        return json.load(f)


def ledger(rec):
    for k in ("url", "repo_url"):
        if rec.get(k):
            rec[k] = clean_url(rec[k])
    rec.setdefault("at", time.strftime("%Y-%m-%dT%H:%M:%S%z"))
    rec.setdefault("ts", time.time())
    rec.setdefault("host", C["host"])
    with open(os.path.join(C["state"], "ledger.jsonl"), "a") as f:
        f.write(json.dumps(rec, sort_keys=True) + "\n")


def read_ledger():
    p = os.path.join(C["state"], "ledger.jsonl")
    if not os.path.exists(p):
        return []
    with open(p) as f:
        return [json.loads(l) for l in f if l.strip()]


def protected_name(p):
    low = p.lower()
    return any(n.lower() in low for n in C["protect_names"])


def kept(p):
    for k in C["keep"]:
        k = k if k.startswith("/") else os.path.join(C["home"], k)
        if p == k or p.startswith(k + "/") or k.startswith(p + "/"):
            return True
    return any(fnmatch.fnmatch(rel(p), g) for g in C["keep_globs"])


def is_regen_dir(parent, name, names_in_parent):
    m = C["regenerable"].get(name)
    if m is None:
        return False
    if m == "":
        return True
    if m.startswith("INSIDE:"):
        return os.path.exists(os.path.join(parent, name, m[7:]))
    return m in names_in_parent


# ---------------------------------------------------------------- live / service paths
def expand_refs(text):
    h = C["home"]
    return text.replace("${HOME}", h).replace("$HOME", h).replace("%h", h).replace("~/", h + "/")


def live_paths():
    """(in_use, service_refs, probes). Raises ProbeError when a probe that must work did not."""
    home = C["home"]
    probes = {}
    in_use = set()
    script = ("for p in /proc/[0-9]*; do for l in cwd root exe; do t=$(readlink $p/$l 2>/dev/null) && echo \"$t\"; done; "
              "for f in $p/fd/*; do t=$(readlink $f 2>/dev/null) && case $t in /*) echo \"$t\";; esac; done; "
              "awk '{print $6}' $p/maps 2>/dev/null | grep '^/' | sort -u; done")
    sudo_ok = bool(shutil.which("sudo")) and subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode == 0
    rc, out, err = run((["sudo", "-n"] if sudo_ok else []) + ["sh", "-c", script], t=900)
    lines = out.splitlines()
    probes["proc"] = {"rc": rc, "lines": len(lines), "all_users": sudo_ok}
    if rc not in (0, 1) or len(lines) < 10:
        raise ProbeError("process scan failed (rc %s, %d lines)" % (rc, len(lines)))
    for l in lines:
        l = l.replace(" (deleted)", "")
        if l.startswith(home + "/"):
            in_use.add(os.path.normpath(l))
    texts = []
    rc, ps, _ = run(["ps", "-eo", "pid=,args="])
    if rc != 0:
        raise ProbeError("ps failed")
    me = str(os.getpid())
    texts.append("\n".join(l for l in ps.splitlines() if l.split(None, 1)[0] != me))
    rc, cron, err = run(["crontab", "-l"])
    probes["crontab"] = rc
    if rc not in (0, 127) and "no crontab" not in err:
        raise ProbeError("crontab -l failed: " + err.strip()[:100])
    texts.append(cron)
    for scope in ([], ["--user"]):
        rc, units, err = run(["systemctl"] + scope + ["list-unit-files", "--no-legend", "--plain"])
        probes["systemctl" + ("-user" if scope else "")] = rc
        if rc == 127:
            continue
        if rc != 0:
            if scope:  # no user bus (ssh/cron): user unit FILES are still read below
                probes["systemctl-user-note"] = "no user bus; unit files read from disk"
                continue
            raise ProbeError("systemctl list-unit-files failed")
        names = [l.split()[0] for l in units.splitlines() if l.strip()]
        rc2, units2, _ = run(["systemctl"] + scope + ["list-units", "--all", "--no-legend", "--plain"])
        names += [l.split()[0] for l in units2.splitlines() if l.strip() and not l.startswith("●")]
        names = sorted(n for n in set(names) if "@." not in n)  # templates cannot be shown; their files are read below
        for i in range(0, len(names), 100):
            rc, o, err = run(["systemctl"] + scope + ["show",
                           "-p", "WorkingDirectory,ExecStart,ExecStartPre,ExecStartPost,ExecStop,ExecReload,Environment,"
                           "EnvironmentFiles,ReadWritePaths,FragmentPath,DropInPaths,RootDirectory", "--"] + names[i:i + 100], t=180)
            if rc != 0:
                raise ProbeError("systemctl show failed: " + err.strip()[:100])
            texts.append(o)
    for g in [home + "/.config/systemd/user/**", "/etc/systemd/system/**", "/etc/systemd/user/**", "/lib/systemd/system/*.service",
              "/etc/cron*/*", "/etc/crontab", "/var/spool/cron/crontabs/*", home + "/Library/LaunchAgents/*.plist",
              "/Library/LaunchAgents/*.plist", "/Library/LaunchDaemons/*.plist", home + "/.bashrc", home + "/.profile",
              home + "/.bash_profile", home + "/.zshrc", home + "/.zprofile", home + "/.config/environment.d/*", "/etc/environment",
              home + "/.config/autostart/*", home + "/.pam_environment", home + "/.local/share/systemd/user/**",
              "/usr/lib/systemd/user/*", "/etc/nginx/**", "/etc/caddy/**", "/etc/apache2/**", "/etc/supervisor/**",
              home + "/.pm2/dump.pm2", "/etc/docker/compose/**"]:
        for f in glob.glob(g, recursive=True):
            if os.path.isfile(f) and os.access(f, os.R_OK):
                try:
                    with open(f, "rb") as fh:
                        texts.append(fh.read(262144).decode("utf-8", "replace"))
                except OSError:
                    pass
    if sudo_ok:  # root and other users' crontabs
        rc, o, err = run(["sudo", "-n", "sh", "-c", "cat /var/spool/cron/crontabs/* 2>/dev/null; true"])
        if rc != 0:
            raise ProbeError("reading root crontabs failed")
        texts.append(o)
    else:
        probes["root_crontabs"] = "not readable without sudo"
    rc, o, err = run(["podman", "ps", "-a", "--format", "{{.Mounts}}"])
    if rc not in (0, 127):
        raise ProbeError("podman ps failed")
    texts.append(o)
    rc, ids, err = run(["docker", "ps", "-aq"])
    probes["docker"] = rc
    if rc not in (0, 127):
        raise ProbeError("docker ps failed: " + err.strip()[:100])
    if rc == 0 and ids.split():
        rc, o, _ = run(["docker", "inspect", "-f", "{{range .Mounts}}{{.Source}} {{end}}{{range $k,$v := .Config.Labels}}{{$v}} {{end}}{{.Config.WorkingDir}}"]
                       + ids.split(), t=300)
        if rc != 0:
            raise ProbeError("docker inspect failed")
        texts.append(o)
    pat = re.compile(re.escape(home) + r"/[^\s\"'<>:,;=\)\]\}\\|&`]+")
    svc = set()
    for t in texts:
        for m in pat.findall(expand_refs(t)):
            m = os.path.normpath(m.rstrip("/."))
            for x in {m, os.path.realpath(m)}:
                svc.add(x)
                if os.path.isfile(x):
                    svc.add(os.path.dirname(x))
    # symlinks in home, ~/bin, ~/.local/bin, ~/.config that point into home (a tool installed from a work folder)
    for d in [home, home + "/bin", home + "/.local/bin", home + "/.config", home + "/.local/share/applications"]:
        try:
            for e in os.scandir(d):
                if e.is_symlink():
                    t = os.path.realpath(e.path)
                    if t.startswith(home + "/"):
                        svc.add(t)
        except OSError:
            pass
    svc.discard(home)
    return in_use, svc, probes


def touches(p, paths):
    pre = p + "/"
    return [x for x in paths if x == p or x.startswith(pre)]


def mounts_under(p):
    out = []
    try:
        with open("/proc/self/mountinfo") as f:
            for l in f:
                mp = l.split()[4].replace("\\040", " ")
                if mp == p or mp.startswith(p + "/"):
                    out.append(mp)
    except OSError:
        pass
    return out


def proc_paths_fast():
    """this user's processes only: cwd/root/exe and open files (used right before each removal)."""
    out = set()
    for p in glob.glob("/proc/[0-9]*"):
        for l in ("cwd", "root", "exe"):
            try:
                out.add(os.readlink(p + "/" + l).replace(" (deleted)", ""))
            except OSError:
                pass
        try:
            for f in os.listdir(p + "/fd"):
                try:
                    t = os.readlink(p + "/fd/" + f)
                    if t.startswith("/"):
                        out.add(t.replace(" (deleted)", ""))
                except OSError:
                    pass
        except OSError:
            pass
    return out


# ---------------------------------------------------------------- units
def find_units():
    """containers' children, plus non-dot top-level folders that are not tool folders. Dot folders never."""
    home = C["home"]
    cont = [os.path.join(home, c) for c in C["containers"]]
    units = []
    for top in sorted(os.listdir(home)):
        p = os.path.join(home, top)
        if os.path.islink(p) or not os.path.isdir(p) or p in cont:
            continue
        if top.startswith(".") or top in C["tool_dirs"]:
            continue
        units.append(p)
    for c in cont:
        if os.path.isdir(c) and not os.path.islink(c):
            for sub in sorted(os.listdir(c)):
                sp = os.path.join(c, sub)
                if os.path.isdir(sp) and not os.path.islink(sp) and not sub.startswith("."):
                    units.append(sp)
    return units


def walk_unit(path):
    """allocated bytes, newest mtime (rebuildable folders and git object stores excluded), repo roots, regen dirs."""
    total, newest = 0, 0.0
    repos, regen = [], []
    dev = os.lstat(path).st_dev
    stack = [(path, False)]
    while stack:
        d, quiet = stack.pop()
        try:
            entries = list(os.scandir(d))
        except OSError:
            continue
        names = {e.name for e in entries}
        if ".git" in names and not quiet:
            repos.append(d)
        for e in entries:
            try:
                st = e.stat(follow_symlinks=False)
            except OSError:
                continue
            total += st.st_blocks * 512
            if stat.S_ISDIR(st.st_mode):
                if st.st_dev != dev:
                    continue
                q = quiet
                if not quiet and is_regen_dir(d, e.name, names):
                    regen.append(e.path); q = True
                if e.name == "objects" and (d.endswith("/.git") or "HEAD" in names):
                    q = True
                stack.append((e.path, q))
            elif not quiet:
                newest = max(newest, st.st_mtime, st.st_ctime)  # ctime: rsync -a / cp -p / tar x keep old mtimes
    return total, newest, repos, regen


# ---------------------------------------------------------------- git facts
LSR = {}


def ls_remote(path, url):
    if url not in LSR:
        rc, o, _ = git(path, "ls-remote", url, t=180)
        LSR[url] = {l.split()[0] for l in o.splitlines() if l.strip()} if rc == 0 else None
    return LSR[url]


def _has_lfs_attr(wt):
    try:
        with open(os.path.join(wt, ".gitattributes"), errors="replace") as fh:
            return "filter=lfs" in fh.read(1 << 20)
    except OSError:
        return False


def repo_facts(wt):
    r = {"path": wt, "linked": os.path.isfile(os.path.join(wt, ".git"))}
    rc, o, _ = git(wt, "rev-parse", "--show-toplevel", "--git-common-dir", "--absolute-git-dir")
    if rc != 0:
        r["state"] = "unreadable"; return r
    top, common, gdir = (o.split("\n") + ["", "", ""])[:3]
    if os.path.realpath(top) != os.path.realpath(wt):
        r["state"] = "nested-non-toplevel"; return r
    r["common_dir"] = os.path.realpath(os.path.join(wt, common)); r["git_dir"] = gdir
    r["submodule"] = "/modules/" in gdir
    rc, rem, _ = git(wt, "remote", "-v")
    remotes = {}
    for l in rem.splitlines():
        f = l.split()
        if len(f) >= 2:
            remotes.setdefault(f[0], f[1])
    origin = remotes.get("origin") or next((u for u in remotes.values() if NET.match(u)), "")
    r["origin"] = clean_url(origin) if NET.match(origin or "") else ""
    r["origin_raw"] = origin if NET.match(origin or "") else ""
    rc, h, _ = git(wt, "rev-parse", "--verify", "-q", "HEAD")
    r["head"] = h.strip() if rc == 0 else ""
    r["branch"] = git(wt, "rev-parse", "--abbrev-ref", "HEAD")[1].strip()
    tips = {}
    if r["head"]:
        tips[r["head"]] = "HEAD"
    if not r["linked"] or r["submodule"]:
        rc, o, _ = git(wt, "for-each-ref", "--format=%(objectname) %(*objectname) %(objecttype) %(refname)")
        for l in o.splitlines():
            f = l.split(" ")
            if f[-1].startswith(("refs/remotes/", "refs/stash")):
                continue
            sha = f[1] or f[0]  # peel annotated tags
            if f[2] in ("commit", "tag") and (f[2] == "commit" or f[1]):
                tips.setdefault(sha, f[-1])
    rc, o, _ = git(wt, "rev-list", "-g", "--max-count=50", "refs/stash")
    r["stashes"] = len(o.split()) if rc == 0 else 0
    r["tips"] = tips
    rc, st, _ = git(wt, "status", "--porcelain=v1", "-z", "-uall", "--ignore-submodules=none", t=300)
    if rc != 0:
        r["state"] = "status-failed"; return r
    ent = [e for e in st.split("\0") if e]
    changes, i = [], 0
    while i < len(ent):
        e = ent[i]; code, path = e[:2], e[3:]
        if code[0] in "RC":
            i += 1
        changes.append((code, path)); i += 1
    r["tracked_changes"] = [p for c, p in changes if c != "??"]
    r["untracked"] = [p for c, p in changes if c == "??"]
    r["signal_changes"] = [p for p in r["tracked_changes"] + r["untracked"] if not NOISE.search(p)]
    rc, o, _ = git(wt, "ls-files", "-v", t=300)
    r["hidden_changes"] = [l[2:] for l in o.splitlines() if l[:1] in "Sabcdefghijklmnopqrstuvwxyz"][:50] if rc == 0 else []
    r["lfs"] = os.path.isdir(os.path.join(r["common_dir"], "lfs")) or _has_lfs_attr(wt)
    r["pending_op"] = any(os.path.exists(os.path.join(gdir, x)) for x in ("rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD", "BISECT_LOG"))
    r["roots"] = git(wt, "rev-list", "--max-parents=0", "HEAD", t=180)[1].split()[:5] if r["head"] else []
    rc, o, _ = git(wt, "worktree", "list", "--porcelain")
    r["worktrees"] = [l[9:] for l in o.splitlines() if l.startswith("worktree ")] if rc == 0 else []
    alt = os.path.join(r["common_dir"], "objects", "info", "alternates")
    r["alternates"] = []
    if os.path.isfile(alt):
        with open(alt, errors="replace") as f:
            for l in f:
                l = l.strip()
                if l and not l.startswith("#"):
                    r["alternates"].append(os.path.normpath(l if l.startswith("/") else os.path.join(r["common_dir"], "objects", l)))
    r["state"] = "ok"
    r["unpushed"] = unpushed_tips(r)
    r["reflog_only"] = reflog_only(r)
    return r


def reflog_only(r):
    """number of commits reachable only from reflogs (lost if objects/ is left out); -1 when unknown."""
    remote = LSR.get(r.get("origin_raw")) if r.get("origin_raw") else set()
    present = []
    if remote:
        vw, _ = objview(r["path"])
        rc, o, _ = git(vw, "cat-file", "--batch-check=%(objectname) %(objecttype)", inp="\n".join(remote).encode(), t=300)
        present = [l.split()[0] for l in o.splitlines() if l.endswith(" commit")]
    inp = "".join("^%s\n" % s for s in list(r["tips"]) + present).encode()
    rc, o, _ = git(r["path"], "rev-list", "--reflog", "--count", "--stdin", inp=inp, t=600)
    try:
        return int(o.strip()) if rc == 0 else -1
    except ValueError:
        return -1


def unpushed_tips(r):
    """tips not reachable from what the origin advertises RIGHT NOW (ls-remote). Stale remote-tracking refs are not
    trusted. ls-remote failure or no network origin -> every tip counts as unpushed."""
    tips = dict(r["tips"])
    if not tips or not r.get("origin_raw"):
        return tips
    remote = ls_remote(r["path"], r["origin_raw"])
    if remote is None:
        r["ls_remote_failed"] = True
        return tips
    tips = {s: n for s, n in tips.items() if s not in remote}
    if not tips:
        return {}
    vw, _ = objview(r["path"])
    rc, o, _ = git(vw, "cat-file", "--batch-check=%(objectname) %(objecttype)", inp="\n".join(remote).encode(), t=300)
    present = [l.split()[0] for l in o.splitlines() if l.endswith(" commit")]
    if not present:
        return tips
    inp = ("\n".join(tips) + "\n" + "\n".join("^" + s for s in present) + "\n").encode()
    rc, o, _ = git(vw, "rev-list", "--stdin", inp=inp, t=900)
    if rc != 0:
        return tips
    local_only = set(o.split())
    return {s: n for s, n in tips.items() if s in local_only}


# ---------------------------------------------------------------- inventory
def disk():
    st = os.statvfs(C["home"])
    return {"total": st.f_blocks * st.f_frsize, "free": st.f_bavail * st.f_frsize}


class UnitIndex:
    """longest-prefix lookup of the unit (or repo) that holds a path (O(depth) per lookup)."""
    def __init__(self, units):
        self.s = set(units)

    def of(self, p):
        while p and p != "/":
            if p in self.s:
                return p
            p = os.path.dirname(p)
        return None


def cmd_inventory(a):
    in_use, svc, probes = live_paths()
    units, allrepos = [], []
    for u in find_units():
        size, newest, repo_paths, regen = walk_unit(u)
        rec = {"path": u, "bytes": size, "newest": newest, "age_hours": round((NOW + C.get("test_clock_offset_s", 0) - newest) / 3600, 1) if newest else None,
               "in_use": sorted(touches(u, in_use))[:20], "service_refs": sorted(touches(u, svc))[:20],
               "protected_name": protected_name(u), "keep": kept(u) or os.path.exists(os.path.join(u, ".keep")),
               "repos": repo_paths, "regen": regen,
               "mounts": mounts_under(u)}
        for g in repo_paths:
            f = repo_facts(g)
            f["unit"] = u
            allrepos.append(f)
        units.append(rec)
        log("unit %-70s %7.2f GiB  age %sh  repos %d" % (rel(u)[:70], size / 2**30, rec["age_hours"], len(repo_paths)))
    alts = set()  # alternates files anywhere under home: repos outside units can borrow objects from a unit
    rc, o, err = run(["find", C["home"], "-xdev", "-path", "*/objects/info/alternates", "-type", "f"], t=1800)
    if rc not in (0, 1):
        raise ProbeError("alternates scan failed: " + err.strip()[:100])
    for f in o.splitlines():
        try:
            with open(f, errors="replace") as fh:
                for l in fh:
                    l = l.strip()
                    if l and not l.startswith("#"):
                        objdir = os.path.dirname(os.path.dirname(f))
                        alts.add((f, os.path.realpath(l if l.startswith("/") else os.path.join(objdir, l))))
        except OSError:
            pass
    logs, media = scan_files({r for u in units for r in u["regen"]})
    inv = {"generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "ts": time.time(), "host": C["host"], "home": C["home"],
           "probes": probes, "disk": disk(), "units": units, "repos": allrepos, "in_use": sorted(in_use),
           "service_refs": sorted(svc), "alternates": sorted([list(x) for x in alts]),
           "log_files": len(logs), "log_bytes": sum(x[0] for x in logs), "media_files": len(media), "media_bytes": sum(x[0] for x in media)}
    jdump(inv, "inventory.json")
    with open(os.path.join(C["state"], "logs.tsv"), "w") as f:
        f.writelines("%d\t%d\t%s\n" % x for x in logs)
    with open(os.path.join(C["state"], "media.tsv"), "w") as f:
        f.writelines("%d\t%d\t%s\n" % x for x in media)
    write_summary(inv)
    log("inventory written to", C["state"])


def scan_files(regen_dirs):
    home = C["home"]
    prune = {os.path.join(home, p) for p in C["file_scan_prune"]}
    logdirs = [os.path.join(home, d) for d in C["log_dirs"]]
    logs, media = [], []
    dev = os.lstat(home).st_dev
    for dp, dn, fn in os.walk(home):
        keep = []
        for d in dn:
            p = os.path.join(dp, d)
            if d in (".git", "site-packages") or p in prune or p in regen_dirs:
                continue
            try:
                if os.lstat(p).st_dev != dev:
                    continue
            except OSError:
                continue
            keep.append(d)
        dn[:] = keep
        for f in fn:
            p = os.path.join(dp, f)
            try:
                st = os.lstat(p)
            except OSError:
                continue
            if not stat.S_ISREG(st.st_mode):
                continue
            if LOG_RE.search(f) or any(p == x or p.startswith(x + "/") for x in logdirs):
                logs.append((st.st_size, int(st.st_mtime), p))
            elif MEDIA_RE.search(f):
                media.append((st.st_size, int(st.st_mtime), p))
    return logs, media


def write_summary(inv):
    G = 2**30
    L = ["# Disk inventory: %s (%s)" % (inv["host"], inv["generated"]), "",
         "Free: %.1f GiB of %.1f GiB. Logs/transcripts: %d files, %.2f GiB. Media: %d files, %.2f GiB." % (
             inv["disk"]["free"] / G, inv["disk"]["total"] / G, inv["log_files"], inv["log_bytes"] / G,
             inv["media_files"], inv["media_bytes"] / G), "", "Probes: " + json.dumps(inv["probes"]), "",
         "| unit | GiB | idle h | repos | in use | service refs | flags |", "|---|---:|---:|---:|---|---|---|"]
    for u in sorted(inv["units"], key=lambda u: -u["bytes"]):
        flags = ",".join(x for x, y in (("protected", u["protected_name"]), ("keep", u["keep"]), ("mount", u["mounts"])) if y)
        L.append("| %s | %.2f | %s | %d | %d | %d | %s |" % (rel(u["path"]), u["bytes"] / G, u["age_hours"], len(u["repos"]),
                                                          len(u["in_use"]), len(u["service_refs"]), flags))
    with open(os.path.join(C["state"], "inventory-summary.md"), "w") as f:
        f.write("\n".join(L) + "\n")


# ---------------------------------------------------------------- plan
def unit_status(u, repos):
    why = []
    if u["protected_name"]:
        why.append("protected name")
    if u["keep"] or kept(u["path"]) or protected_name(u["path"]):
        why.append("keep list")
    if u["in_use"]:
        why.append("open files / process cwd inside")
    if u["service_refs"]:
        why.append("referenced by a service, cron job or container")
    if u.get("mounts"):
        why.append("mount point inside")
    if u["age_hours"] is None:
        why.append("no dated files")
    elif u["age_hours"] < C["active_hours"]:
        why.append("touched within %dh" % C["active_hours"])
    for r in repos:
        if r.get("linked") and r.get("common_dir") and not inside(r["common_dir"], u["path"]) and r.get("reflog_only", -1) != 0:
            why.append("linked worktree %s has reflog-only commits in a store outside the unit" % rel(r["path"]))
        if r.get("state") != "ok":
            why.append("repo %s: %s" % (rel(r["path"]), r.get("state")))
        elif r.get("pending_op"):
            why.append("repo %s: rebase/merge in progress" % rel(r["path"]))
    return why


def inside(p, root):
    return p == root or p.startswith(root + "/")


def dependency_keeps(inv, keep):
    """grow the keep set until stable: a unit stays when something outside the wipe set needs a path inside it
    (a worktree's object store, an alternates object dir), or when it is a worktree whose store is kept-and-pruned."""
    units = [u["path"] for u in inv["units"]]
    idx = UnitIndex(units)
    deps = []  # (needer_path, needed_path)
    for r in inv["repos"]:
        cd = r.get("common_dir")
        if cd:
            deps.append((r["path"], cd))
            for w in r.get("worktrees", []):
                deps.append((w, cd))
        for a in r.get("alternates", []):
            deps.append((r["path"], a))
    for f, t in inv.get("alternates", []):
        deps.append((f, t))
    reasons = {}
    changed = True
    while changed:
        changed = False
        for needer, needed in deps:
            nu, du = idx.of(needer), idx.of(needed)
            if du is None or du in keep or nu == du:
                continue
            if nu is None or nu in keep:  # needer outside every unit (always stays) or in a kept unit
                keep.add(du); changed = True
                reasons[du] = "holds git objects used by %s" % rel(needer)
    return reasons


def cmd_plan(a):
    inv = jload("inventory.json")
    by_unit = {}
    for r in inv["repos"]:
        by_unit.setdefault(r["unit"], []).append(r)
    status = {u["path"]: unit_status(u, by_unit.get(u["path"], [])) for u in inv["units"]}
    keep = {p for p, w in status.items() if w}
    for p, why in dependency_keeps(inv, keep).items():
        status[p].append(why)
    plan = []
    for u in inv["units"]:
        why = status[u["path"]]
        work = [{"repo": r["path"], "origin": r.get("origin") or "(none)", "unpushed": len(r.get("unpushed", {})),
                 "changes": len(r.get("signal_changes", [])), "stashes": r.get("stashes", 0)}
                for r in by_unit.get(u["path"], []) if r.get("state") == "ok" and (r.get("unpushed") or r.get("signal_changes") or r.get("stashes"))]
        plan.append({"unit": u["path"], "bytes": u["bytes"], "age_hours": u["age_hours"],
                     "action": "keep" if why else "secure-then-wipe", "why": why, "recover": work})
    jdump(plan, "plan.json")
    G = 2**30
    w = [p for p in plan if p["action"] != "keep"]
    k = [p for p in plan if p["action"] == "keep"]
    lines = ["prunedisk-ops plan (%s, %s)" % (C["host"], C["date"]),
             "  secure-then-wipe: %d units, %.1f GiB (%d repos with local-only work)" % (
                 len(w), sum(p["bytes"] for p in w) / G, sum(len(p["recover"]) for p in w)),
             "  keep:             %d units, %.1f GiB" % (len(k), sum(p["bytes"] for p in k) / G)]
    reasons = {}
    for p in k:
        key = p["why"][0].split(":")[0]
        reasons.setdefault(key, [0, 0]); reasons[key][0] += 1; reasons[key][1] += p["bytes"]
    for key, (n, b) in sorted(reasons.items(), key=lambda x: -x[1][1]):
        lines.append("      %4d %8.1f GiB  %s" % (n, b / G, key))
    for p in sorted(w, key=lambda p: -p["bytes"]):
        lines.append("    wipe %7.2f GiB  %s  (recover %d)" % (p["bytes"] / G, rel(p["unit"]), len(p["recover"])))
    txt = "\n".join(lines)
    with open(os.path.join(C["state"], "plan.txt"), "w") as f:
        f.write(txt + "\n")
    print("\n".join(lines[:40]))


# ---------------------------------------------------------------- recover
NOISE_REPO = re.compile(r"/(\.scratch|tmp|fixtures?|testdata|sandbox|mutants?)/|-fixture", re.I)
CENV = dict(GENV_LOCAL, GIT_AUTHOR_NAME="prunedisk-ops", GIT_AUTHOR_EMAIL="prunedisk-ops@localhost",
            GIT_COMMITTER_NAME="prunedisk-ops", GIT_COMMITTER_EMAIL="prunedisk-ops@localhost")


def secret_path(p):
    b = os.path.basename(p)
    return bool(SECRET_NAME.match(b)) and not SECRET_OK.search(b)


def snapshot_commit(r):
    """commit (on no ref) holding HEAD + tracked changes + untracked non-ignored files, minus secret-looking names,
    noise and files > 50 MB. Working tree, index and refs are untouched."""
    wt = r["path"]
    tmpd = tempfile.mkdtemp(prefix="pdo-idx-")
    env = dict(GENV_LOCAL, GIT_INDEX_FILE=os.path.join(tmpd, "index"))
    try:
        if r["head"] and git(wt, "read-tree", r["head"], env=env)[0] != 0:
            return None, "read-tree failed"
        add, rm = [], []
        for p in list(r["tracked_changes"]) + list(r["untracked"]):
            if NOISE.search(p) or secret_path(p) or LOG_RE.search(os.path.basename(p)) or MEDIA_RE.search(p) or OPAQUE_RE.search(p):
                continue  # logs and media go to their own verified archives
            fp = os.path.join(wt, p)
            if not os.path.lexists(fp):
                rm.append(p); continue
            try:
                if os.path.isfile(fp) and not os.path.islink(fp):
                    if os.path.getsize(fp) > 50 << 20:
                        continue
                    with open(fp, "rb") as fh:
                        if SECRET_CONTENT.search(fh.read()):
                            continue
            except OSError:
                continue
            add.append(p)
        if not add and not rm:
            return None, "no significant changes"
        for i in range(0, len(add), 500):
            rc, _, err = git(wt, "add", "-f", "--", *add[i:i + 500], env=env, t=600)
            if rc != 0:
                return None, "git add failed: " + err.strip()[-200:]
        for i in range(0, len(rm), 500):
            git(wt, "rm", "--cached", "-q", "--ignore-unmatch", "--", *rm[i:i + 500], env=env)
        rc, tree, _ = git(wt, "write-tree", env=env)
        tree = tree.strip()
        if rc != 0 or not tree:
            return None, "write-tree failed"
        if r["head"] and git(wt, "rev-parse", r["head"] + "^{tree}")[1].strip() == tree:
            return None, "snapshot equals HEAD"
        msg = "recovery snapshot of ~/%s on %s at %s\n\nUncommitted work saved by prunedisk-ops before the copy was removed." % (
            rel(wt), C["host"], time.strftime("%Y-%m-%d %H:%M %z"))
        rc, sha, _ = git(wt, "commit-tree", tree, "-m", msg, *(["-p", r["head"]] if r["head"] else []), env=CENV)
        return (sha.strip(), "") if rc == 0 else (None, "commit-tree failed")
    finally:
        shutil.rmtree(tmpd, ignore_errors=True)


def push_check(wt, shas, remote_shas):
    """scan every object the push would add: secret-looking paths, secret-looking content, total size."""
    present = []
    vw, is_view = objview(wt)  # objects missing from a partial clone are on its promisor remote already
    if remote_shas:
        rc, o, _ = git(vw, "cat-file", "--batch-check=%(objectname) %(objecttype)", inp="\n".join(remote_shas).encode(), t=300)
        present = [l.split()[0] for l in o.splitlines() if l.endswith(" commit")]
    inp = ("\n".join(shas) + "\n" + "".join("^%s\n" % s for s in present)).encode()
    rc, o, _ = git(vw, "rev-list", "--objects", *(["--missing=allow-any"] if is_view else []), "--stdin", inp=inp, t=1800)
    if rc != 0:
        return False, "rev-list --objects failed"
    objs, names = [], {}
    for l in o.splitlines():
        f = l.split(" ", 1)
        objs.append(f[0])
        if len(f) > 1:
            names[f[0]] = f[1]
            if secret_path(f[1]):
                return False, "secret-looking file in new history: %s" % f[1]
    rc, o, _ = git(vw, "cat-file", "--batch-check=%(objectname) %(objecttype) %(objectsize)", inp="\n".join(objs).encode(), t=900)
    total, blobs = 0, []
    for l in o.splitlines():
        f = l.split()
        if len(f) == 3:
            total += int(f[2])
            if f[1] in ("blob", "commit", "tag"):
                if int(f[2]) > 8 << 20:
                    return False, "new blob over 8 MiB (%s): archived instead of pushed" % names.get(f[0], f[0][:10])
                blobs.append(f[0])
    if total > C["max_push_mb"] << 20:
        return False, "push would add %d MiB (> max_push_mb)" % (total >> 20)
    for i in range(0, len(blobs), 2000):
        p = subprocess.run(["git", "-C", vw] + GSAFE + ["cat-file", "--batch"], input="\n".join(blobs[i:i + 2000]).encode(),
                           capture_output=True, env=GENV_LOCAL, timeout=1800)
        if p.returncode != 0:
            return False, "cat-file --batch failed"
        if SECRET_CONTENT.search(p.stdout):
            return False, "secret-looking content in new history"
    return True, "%d objects, %d KiB" % (len(objs), total >> 10)


def remote_branch_exists(wt, url, ref):
    rc, o, _ = git(wt, "ls-remote", url, ref, t=120)
    if rc != 0:
        return "?"
    return o.split()[0] if o.strip() else None


def push_ref(wt, url, sha, branch):
    ref = "refs/heads/" + branch
    existing = remote_branch_exists(wt, url, ref)
    if existing == "?":
        return False, "ls-remote failed"
    if existing == sha:
        return True, "already there"
    if existing:
        return False, "branch exists with a different commit"
    rc, out, err = git(wt, "push", "--no-verify", "--quiet", url, "%s:%s" % (sha, ref), t=1800)
    if rc != 0:
        return False, clean_url(err.strip()[-300:])
    got = remote_branch_exists(wt, url, ref)
    return (got == sha), ("pushed" if got == sha else "push not visible on the remote")


VIS = {}


def visibility(url):
    if url not in VIS:
        m = re.search(r"github\.com[:/]([^/]+/[^/\s]+?)(\.git)?/?$", url)
        v = run(["gh", "repo", "view", m.group(1), "--json", "visibility", "-q", ".visibility"])[1].strip() if m else ""
        VIS[url] = v or C["visibility_default"]
    return VIS[url]


def map_origin(r, root_index):
    """no-origin work always goes to the owner's salvage repo (a repo sharing a root commit may be a third party's)."""
    return C["salvage_repo"], "salvage"


def cmd_recover(a):
    inv = jload("inventory.json")
    plan = jload("plan.json")
    targets = {p["unit"] for p in plan if p["action"] == "secure-then-wipe"}
    if a.include_kept:
        targets |= {p["unit"] for p in plan}
    root_index = {}
    for r in inv["repos"]:
        if r.get("origin_raw") and r.get("state") == "ok":
            for root in r.get("roots", []):
                root_index.setdefault(root, r["origin_raw"])
    done = {(l["url"], l["sha"]): l["branch"] for l in read_ledger() if l.get("kind") == "recovery" and l.get("ok")}
    results = []
    for r in inv["repos"]:
        if r["unit"] not in targets or r.get("state") != "ok" or protected_name(r["path"]):
            continue
        if not os.path.isdir(r["path"]):
            continue
        if r.get("stashes"):
            results.append({"repo": r["path"], "note": "%d stash entries: not pushed, repo archived whole" % r["stashes"]})
        if not r.get("origin_raw") and NOISE_REPO.search(r["path"] + "/"):
            results.append({"repo": r["path"], "skip": "test fixture / scratch repo: archived in the residue, not pushed"}); continue
        url, how = (r["origin_raw"], "origin") if r.get("origin_raw") else map_origin(r, root_index)
        if not url:
            results.append({"repo": r["path"], "skip": "no origin and no salvage repo configured"}); continue
        unpushed = dict(r.get("unpushed", {})) if r.get("origin_raw") else dict(r.get("tips", {}))
        if not unpushed and not r.get("signal_changes"):
            continue
        vis = visibility(url)
        if vis != "PRIVATE":
            results.append({"repo": r["path"], "url": clean_url(url), "skip": "target visibility %s: not pushed, archived in the residue" % vis}); continue
        LSR.pop(url, None)
        remote = ls_remote(r["path"], url)  # fresh
        if remote is None:
            results.append({"repo": r["path"], "skip": "ls-remote failed: archived in the residue"}); continue
        # one branch per independent tip (tips that are ancestors of another tip are carried by it)
        tips = list(unpushed)
        if len(tips) > 1:
            rc, o, _ = git(r["path"], "merge-base", "--independent", *tips)
            if rc == 0 and o.split():
                keepset = set(o.split())
                unpushed = {s: n for s, n in unpushed.items() if s in keepset}
        items = []
        desc = slug(rel(r["path"]).replace("/", "-"), 70)
        for sha, name in unpushed.items():
            nm = "head" if name == "HEAD" else slug(name.replace("refs/heads/", "").replace("refs/tags/", "tag-").replace("refs/notes/", "notes-"), 30)
            items.append((sha, "recovery/%s-%s-%s-%s-%s" % (C["host"], C["date"], desc, nm, sha[:8])))
        if r.get("signal_changes"):
            sha, why = snapshot_commit(r) if a.apply else ("<snapshot>", "")
            if sha:
                items.append((sha, "recovery/%s-%s-%s-wip-%s" % (C["host"], C["date"], desc, sha[:8])))
            else:
                results.append({"repo": r["path"], "note": "snapshot skipped: " + why})
        if len(items) > C["max_branches_per_repo"]:
            results.append({"repo": r["path"], "skip": "%d branches needed (> max_branches_per_repo): archived whole" % len(items)}); continue
        if a.apply and items:
            ok, st = push_check(r["path"], [s for s, _ in items], sorted(remote or []))
            if not ok:
                results.append({"repo": r["path"], "url": clean_url(url), "skip": st + ": not pushed, archived in the residue"})
                ledger({"kind": "recovery-skip", "repo": r["path"], "url": url, "why": st, "ok": False})
                continue
        for sha, branch in items:
            if (clean_url(url), sha) in done:
                rec = {"kind": "recovery", "repo": r["path"], "url": url, "via": how, "sha": sha, "branch": done[(clean_url(url), sha)],
                       "ok": True, "status": "dedup"}
                if a.apply:
                    ledger(rec)
                results.append(rec); continue
            if not a.apply:
                results.append({"repo": r["path"], "url": clean_url(url), "sha": sha, "branch": branch, "status": "dry-run"})
                log("would push %s -> %s" % (rel(r["path"]), branch)); continue
            ok, st = push_ref(r["path"], url, sha, branch)
            n = None
            if ok:
                pres = [s for s in (remote or [])]
                inp = (sha + "\n" + "".join("^%s\n" % s for s in pres)).encode()
                rc, o, _ = git(r["path"], "rev-list", "--count", "--stdin", "--ignore-missing", inp=inp, t=600)
                n = int(o.strip() or 0) if rc == 0 else None
            rec = {"kind": "recovery", "repo": r["path"], "url": url, "via": how, "sha": sha, "branch": branch, "ok": ok, "status": st, "commits": n}
            ledger(rec)
            if ok:
                done[(clean_url(url), sha)] = branch
            results.append(rec)
            log("PUSH %s -> %s %s" % (rel(r["path"]), branch, st))
    jdump(results, "recover-%s.json" % ("apply" if a.apply else "dry"))
    print("recover: %d actions (%s)" % (len(results), "applied" if a.apply else "dry run"))


# ---------------------------------------------------------------- backup
ZSTD = shutil.which("zstd")


def sha_file(p, algo="sha256"):
    h = hashlib.new(algo)
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def remote_info(dest):
    rc, o, err = run(["rclone", "lsjson", "--hash", "--files-only", dest], t=900)
    if rc != 0:
        if "not found" in err.lower() or "directory not found" in err.lower() or "object not found" in err.lower():
            return []
        return None
    try:
        return json.loads(o or "[]")
    except ValueError:
        return None


def put_verified(local, key):
    """upload one local file to <remote>/<key>; refuse to overwrite; verify size and storage SHA-1."""
    dest = C["remote"].rstrip("/") + "/" + key
    info = remote_info(dest)
    if info is None:
        return {"ok": False, "why": "cannot list destination", "dest": dest}
    if info:
        return {"ok": False, "why": "object already exists (refusing to overwrite)", "dest": dest}
    size, s1, s256 = os.path.getsize(local), sha_file(local, "sha1"), sha_file(local)
    rc, _, err = run(["rclone", "copyto", "--retries", "3", "--b2-chunk-size", "96M", local, dest], t=6 * 3600)
    if rc != 0:
        return {"ok": False, "why": "rclone copyto failed: " + (err.strip().splitlines() or ["?"])[-1][:200], "dest": dest}
    info = remote_info(dest)
    res = {"dest": dest, "bytes": size, "sha256": s256, "sha1": s1, "ok": False}
    if not info or len(info) != 1:
        res["why"] = "object not listed after upload"; return res
    if info[0].get("Size") != size:
        res["why"] = "size mismatch"; return res
    hs = {k.lower().replace("-", ""): (v or "").lower() for k, v in (info[0].get("Hashes") or {}).items()}
    if hs.get("sha1") != s1:
        res["why"] = "storage SHA-1 %s does not match local" % (hs.get("sha1") or "missing"); return res
    res["ok"] = True
    res["verify"] = "size+sha1"
    return res


def archive(members, key, base, extra_index=None):
    """tar(+zstd) members (absolute paths, stored relative to base) into a local temp file, test it, upload it and its
    index, verify both. members: list of paths (dirs are added non-recursively)."""
    tmpd = tempfile.mkdtemp(prefix="pdo-ar-", dir=os.path.join(C["state"], "tmp"))
    try:
        tpath = os.path.join(tmpd, "a.tar")
        index, problems, raw = [], [], 0
        with tarfile.open(tpath, "w", format=tarfile.PAX_FORMAT) as tf:
            for m in members:
                try:
                    st = os.lstat(m)
                    tf.add(m, arcname=os.path.relpath(m, base), recursive=False)
                    if stat.S_ISREG(st.st_mode):
                        raw += st.st_size
                    index.append({"path": m, "size": st.st_size, "mtime": int(st.st_mtime),
                                  "type": "d" if stat.S_ISDIR(st.st_mode) else ("l" if stat.S_ISLNK(st.st_mode) else "f")})
                except (OSError, ValueError) as e:
                    if os.path.lexists(m):
                        problems.append("tar %s: %s" % (m, type(e).__name__))
                    elif "/objects/" in m:  # a repack moved objects between listing and tar: the archive would miss them
                        problems.append("git object vanished during archive (repack?): %s" % m)
        if problems:
            return {"ok": False, "why": problems[0], "key": key, "problems": len(problems)}
        if ZSTD:
            if run([ZSTD, "-T0", "-3", "-q", "--rm", tpath, "-o", tpath + ".zst"], t=6 * 3600)[0] != 0:
                return {"ok": False, "why": "zstd failed", "key": key}
            if run([ZSTD, "-t", "-q", tpath + ".zst"], t=6 * 3600)[0] != 0:
                return {"ok": False, "why": "zstd -t failed", "key": key}
            tpath += ".zst"
        elif key.endswith(".zst"):
            key = key[:-4]
        res = put_verified(tpath, key)
        res.update(key=key, raw_bytes=raw, files=len(members))
        if not res["ok"]:
            return res
        ipath = os.path.join(C["state"], "indexes", key.replace("/", "__") + ".index.jsonl")
        os.makedirs(os.path.dirname(ipath), exist_ok=True)
        with open(ipath, "w") as f:
            for row in index + list(extra_index or []):
                f.write(json.dumps(row, sort_keys=True) + "\n")
        ires = put_verified(ipath, key + ".index.jsonl")
        res["index_local"] = ipath
        res["index_dest"] = ires.get("dest")
        if not ires["ok"]:
            res["ok"] = False; res["why"] = "index upload: " + ires.get("why", "?")
        return res
    finally:
        shutil.rmtree(tmpd, ignore_errors=True)


def tracked_clean_files(r):
    rc, o, _ = git(r["path"], "ls-files", "-z", t=300)
    if rc != 0:
        return set()
    changed = set(r.get("tracked_changes", [])) | set(r.get("hidden_changes", []))
    return {os.path.join(r["path"], p) for p in o.split("\0") if p and p not in changed}


def secured_repo(r, led):
    """every local-only tip reached a private remote this run (ledger) and nothing lives outside commits
    (no stash, no LFS objects, no skip-worktree/assume-unchanged edits, no submodule metadata)."""
    if r.get("state") != "ok" or r.get("stashes") or r.get("lfs") or r.get("hidden_changes") or r.get("submodule"):
        return False
    if not r.get("origin_raw") or r.get("ls_remote_failed"):
        need = set(r.get("tips", {}))
    else:
        need = set(r.get("unpushed", {}))
    if r.get("reflog_only", -1) != 0:
        return False
    pushed = {l["sha"] for l in led if l.get("kind") == "recovery" and l.get("ok") and l.get("repo") == r["path"]}
    if not need:
        return bool(r.get("origin_raw")) and not r.get("ls_remote_failed")
    for t in need - pushed:  # a tip carried by a pushed recovery branch counts as pushed
        if not any(git(r["path"], "merge-base", "--is-ancestor", t, p)[0] == 0 for p in pushed):
            return False
    return True


def has_secret_content(p):
    try:
        with open(p, "rb") as fh:
            return bool(SECRET_CONTENT.search(fh.read(1 << 20)))
    except OSError:
        return False


def regen_ok(d, repos):
    """a rebuildable folder may be left out only if git tracks nothing in it and it holds no repo."""
    for dp, dn, fn in os.walk(d):
        if ".git" in dn or ".git" in fn:
            return False
    for r in repos:
        if inside(d, r["path"]) and d != r["path"]:
            rc, o, _ = git(r["path"], "ls-files", "--", os.path.relpath(d, r["path"]), t=300)
            if rc != 0 or o.strip():
                return False
    return True


def archived_paths(led):
    """paths saved in verified archives (from their local index copies)."""
    out = set()
    for l in led:
        if l.get("kind") in ("logs", "media", "dir") and l.get("ok") and l.get("index_local") and os.path.exists(l["index_local"]):
            with open(l["index_local"]) as f:
                for line in f:
                    row = json.loads(line)
                    if row.get("type") not in ("f", "dup"):
                        continue
                    p = row["dup_path"] if row.get("type") == "dup" else row["path"]
                    try:
                        st = os.lstat(p)
                    except OSError:
                        continue
                    if st.st_size == row.get("size") and int(st.st_mtime) == row.get("mtime") and st.st_ctime < l.get("ts", 0):
                        out.add(p)
    return out


def cmd_backup(a):
    inv = jload("inventory.json")
    plan = jload("plan.json")
    os.makedirs(os.path.join(C["state"], "tmp"), exist_ok=True)
    led = read_ledger()
    done_keys = {l["key"] for l in led if l.get("kind") in ("logs", "media") and l.get("ok")}
    repos_ok = [r for r in inv["repos"] if r.get("state") == "ok"]
    ridx = UnitIndex([r["path"] for r in inv["repos"]])
    rmap = {r["path"]: r for r in inv["repos"]}
    uidx = UnitIndex([u["path"] for u in inv["units"]])
    wipe_units = [p["unit"] for p in plan if p["action"] == "secure-then-wipe"]
    pre = "%s/%s" % (C["host"], C["date"])
    total = [0]
    budget = a.max_gb * 2**30
    vault = os.path.join(C["state"], "vault")

    def go(kind, members, key, base, extra, extra_index=None):
        if key in done_keys:
            return True
        size = sum(os.lstat(m).st_size for m in members if os.path.lexists(m))
        if not a.apply:
            log("would upload %-8s %9.1f MiB %6d files -> %s" % (kind, size / 2**20, len(members), key)); total[0] += size; return True
        if total[0] + size > budget:
            log("budget reached; skipping", key); return False
        res = archive(members, key, base, extra_index)
        res.update(kind=kind, base=base, **extra)
        res["paths"] = [rel(m) for m in members][:200]
        res["paths_total"] = len(members)
        ledger(res); led.append(res)
        total[0] += size
        log("%s %-8s %9.1f MiB -> %s %s" % ("OK " if res["ok"] else "ERR", kind, res.get("bytes", 0) / 2**20, key, res.get("why", "")))
        return res["ok"]

    def owner_url(p):
        rp = ridx.of(p)
        return (rmap[rp].get("origin") if rp else "") or ""

    # 1. logs / transcripts / conversations, grouped by unit (or top two levels for the rest), all units incl. active
    groups = {}
    with open(os.path.join(C["state"], "logs.tsv")) as f:
        for l in f:
            s, m, p = l.rstrip("\n").split("\t", 2)
            if secret_path(p):
                continue
            g = uidx.of(p) or os.path.join(C["home"], "/".join(rel(p).split("/")[:2]))
            groups.setdefault(g, []).append(p)
    for g, members in sorted(groups.items()):
        go("logs", sorted(members), "%s/logs/%s.tar.zst" % (pre, keyname(g)), C["home"], {"group": rel(g), "repo_url": owner_url(g)})
    # 2. media not recoverable from a secured repo, deduplicated by content (duplicates listed in the index)
    seen, mgroups, dups, hstat = {}, {}, {}, {}
    tracked_cache = {}
    with open(os.path.join(C["state"], "media.tsv")) as f:
        for l in f:
            s, m, p = l.rstrip("\n").split("\t", 2)
            rp = ridx.of(p)
            if rp and secured_repo(rmap[rp], led):
                if rp not in tracked_cache:
                    tracked_cache[rp] = tracked_clean_files(rmap[rp])
                if p in tracked_cache[rp]:
                    continue
            try:
                st0 = os.lstat(p)
                h = sha_file(p)
                st1 = os.lstat(p)
            except OSError:
                continue
            if (st0.st_size, st0.st_mtime_ns) != (st1.st_size, st1.st_mtime_ns):
                h = "changed-while-hashing:" + p  # never treat a file rewritten mid-hash as a duplicate
            hstat[p] = (st0.st_size, int(st0.st_mtime))
            if h in seen:
                dups.setdefault(seen[h], []).append(p); continue
            seen[h] = p
            mgroups.setdefault(uidx.of(p) or os.path.join(C["home"], "/".join(rel(p).split("/")[:2])), []).append(p)
    for g, members in sorted(mgroups.items()):
        extra_index = []
        for o in members:
            for d in dups.get(o, []):
                size, mtime = hstat[d]  # stat taken at hash time, so a later rewrite no longer matches this row
                extra_index.append({"path": o, "dup_path": d, "type": "dup", "size": size, "mtime": mtime})
        go("media", sorted(members), "%s/media/%s.tar.zst" % (pre, keyname(g)), C["home"],
           {"group": rel(g), "repo_url": owner_url(g)}, extra_index)
    jdump({"media_unique": len(seen), "media_dups": sum(len(v) for v in dups.values())}, "media-dedupe.json")
    jdump({"groups": sorted({rel(g) for g in groups} | {rel(g) for g in mgroups})}, "expected-groups.json")
    # 3. residue of each wipe unit
    saved = archived_paths(led)
    fresh = {l["unit"] for l in led if l.get("kind") == "residue" and l.get("ok") and NOW - l.get("started", 0) < C["max_backup_age_hours"] * 3600}
    for u in wipe_units:
        if rel(u) in fresh or not os.path.isdir(u):
            continue
        started = time.time()
        rs = []
        for r0 in [r for r in inv["repos"] if r["unit"] == u]:  # fresh facts: anything since the inventory counts
            if os.path.isdir(r0["path"]):
                f = repo_facts(r0["path"]); f["unit"] = u
                rs.append(f)
        # repos created since the inventory are found too
        known = {r["path"] for r in rs}
        for g in walk_unit(u)[2]:
            if g not in known:
                f = repo_facts(g); f["unit"] = u; rs.append(f)
        if any(r.get("linked") and r.get("common_dir") and not inside(r["common_dir"], u) and r.get("reflog_only", -1) != 0 for r in rs):
            if a.apply:
                ledger({"kind": "residue", "key": "", "unit": rel(u), "ok": False, "why": "linked worktree with reflog-only commits"})
            continue
        if any(r.get("state") != "ok" for r in rs):
            if a.apply:
                ledger({"kind": "residue", "key": "", "unit": rel(u), "ok": False, "why": "a repo is unreadable now"})
            continue
        rs = [r for r in rs if r.get("state") == "ok"]
        recoverable, git_skip = set(), set()
        for r in rs:
            if secured_repo(r, led) or (not a.apply and not r.get("unpushed") and r.get("origin_raw")):
                recoverable |= tracked_clean_files(r)
                gd = r.get("common_dir", "")
                if inside(gd, u):
                    git_skip |= {os.path.join(gd, "objects"), os.path.join(gd, "lfs")}
        regen_skip = {d for d in next((x["regen"] for x in inv["units"] if x["path"] == u), []) if regen_ok(d, rs)}
        members, secrets, size = [], [], 0
        for dp, dn, fn in os.walk(u):
            members.append(dp)
            keepd = []
            for d in dn:
                p = os.path.join(dp, d)
                if p in regen_skip or p in git_skip:
                    continue
                if os.path.islink(p):
                    members.append(p); continue
                keepd.append(d)
            dn[:] = keepd
            for fname in fn:
                p = os.path.join(dp, fname)
                if p in recoverable or p in saved:
                    continue
                try:
                    st = os.lstat(p)
                except OSError:
                    continue
                if not (stat.S_ISREG(st.st_mode) or stat.S_ISLNK(st.st_mode)):
                    continue
                if stat.S_ISREG(st.st_mode) and (secret_path(p) or (st.st_size <= 1 << 20 and has_secret_content(p))):
                    secrets.append(p); continue
                members.append(p); size += st.st_size
        if size > C["max_residue_gb"] * 2**30:
            if a.apply:
                ledger({"kind": "residue", "key": "", "unit": rel(u), "ok": False, "why": "residue %.1f GiB over max_residue_gb" % (size / 2**30)})
            log("residue too big (%.1f GiB), unit will be kept: %s" % (size / 2**30, rel(u)))
            continue
        vrows = []
        if a.apply and secrets:
            for p in secrets:
                dst = os.path.join(vault, rel(p))
                os.makedirs(os.path.dirname(dst), mode=0o700, exist_ok=True)
                shutil.copy2(p, dst)
                os.chmod(dst, 0o600)
                vrows.append({"path": p, "vault": dst, "sha256": sha_file(dst), "type": "vault"})
            os.chmod(vault, 0o700)
        elif secrets:
            log("would copy %d secret-named files of %s to the local vault" % (len(secrets), rel(u)))
        key = "%s/residue/%s-%s.tar.zst" % (pre, keyname(u), time.strftime("%H%M%S"))
        r0 = rs[0] if rs else None
        if a.apply:
            ok_v = all(sha_file(v["path"]) == v["sha256"] for v in vrows)
            res = archive(members, key, C["home"], vrows)
            res.update(kind="residue", unit=rel(u), started=started, repo_url=(r0 or {}).get("origin", ""),
                       vault_files=len(vrows), paths=[rel(m) for m in members][:200], paths_total=len(members),
                       secured_repos=[rel(r["path"]) for r in rs if secured_repo(r, led)])
            if not ok_v:
                res["ok"] = False; res["why"] = "vault copy mismatch"
            ledger(res); led.append(res)
            total[0] += size
            log("%s residue %9.1f MiB -> %s %s" % ("OK " if res["ok"] else "ERR", res.get("bytes", 0) / 2**20, key, res.get("why", "")))
        else:
            log("would upload residue %9.1f MiB %6d files -> %s" % (size / 2**20, len(members), key)); total[0] += size
    print("backup: %.2f GiB %s" % (total[0] / 2**30, "uploaded" if a.apply else "would be uploaded"))


def cmd_backup_dir(a):
    """upload one folder as a single verified archive (e.g. a salvage folder)."""
    os.makedirs(os.path.join(C["state"], "tmp"), exist_ok=True)
    p = os.path.realpath(a.path)
    members = []
    for dp, dn, fn in os.walk(p):
        members.append(dp)
        members += [os.path.join(dp, d) for d in dn if os.path.islink(os.path.join(dp, d))]
        members += [os.path.join(dp, f) for f in fn]
    key = "%s/%s/dirs/%s.tar.zst" % (C["host"], C["date"], keyname(p))
    size = sum(os.lstat(m).st_size for m in members if os.path.lexists(m))
    if not a.apply:
        print("would upload %s (%.2f GiB, %d entries) -> %s" % (p, size / 2**30, len(members), key)); return
    res = archive(members, key, os.path.dirname(p))
    res.update(kind="dir", unit=rel(p), group=rel(p))
    ledger(res)
    print(json.dumps({k: res.get(k) for k in ("ok", "why", "dest", "bytes", "sha256", "files")}))


# ---------------------------------------------------------------- manifests
README = ("# Backup manifests\n\nEach `<host>-<date>.jsonl` lists what prunedisk-ops saved from that host before it cleaned the disk.\n"
          "- `kind` logs | media | residue | dir: a verified archive at `storage_key` (an rclone path) with its `sha256`, `bytes` and\n"
          "  storage SHA-1; `index_key` lists every file inside it. Restore: `rclone cat <storage_key> | zstd -d | tar -x`.\n"
          "- `kind` recovery: local-only git work pushed to `branch` at `commit` (never merged; delete the branch when done).\n"
          "Secret-named files were not uploaded: they stay in the host's local vault (`~/.prunedisk-ops/<date>/vault`).\n")


def manifest_row(l):
    return {"kind": l["kind"], "host": l.get("host"), "date": C["date"], "at": l.get("at"),
            "disk_path": l.get("group") or l.get("unit") or (rel(l["repo"]) if l.get("repo") else ""),
            "storage_key": l.get("dest") or "", "index_key": l.get("index_dest") or "", "bytes": l.get("bytes"),
            "raw_bytes": l.get("raw_bytes"), "sha256": l.get("sha256"), "sha1": l.get("sha1"), "files": l.get("files"),
            "branch": l.get("branch"), "commit": l.get("sha") if l["kind"] == "recovery" else None, "commits": l.get("commits"),
            "repo_url": clean_url(l.get("url") or l.get("repo_url") or ""), "paths_sample": (l.get("paths") or [])[:50]}


def cmd_manifests(a):
    led = [l for l in read_ledger() if l.get("ok") and l.get("kind") in ("logs", "media", "residue", "dir", "recovery")]
    raw_url = {}
    for r in jload("inventory.json")["repos"]:
        if r.get("origin"):
            raw_url[r["origin"]] = r["origin_raw"]
    by_url = {}
    for l in led:
        u = clean_url(l.get("url") or l.get("repo_url") or "")
        if u:
            by_url.setdefault(u, []).append(l)
    out = []
    fname = "backup-manifests/%s-%s.jsonl" % (C["host"], C["date"])
    targets = [(u, recs) for u, recs in sorted(by_url.items()) if u != clean_url(C["index_repo"])]
    if C["index_repo"]:
        targets.append((clean_url(C["index_repo"]), led))
    for url, recs in targets:
        push_url = raw_url.get(url) or (C["index_repo"] if url == clean_url(C["index_repo"]) else url)
        vis = visibility(push_url)
        if vis != "PRIVATE":
            out.append({"url": url, "rows": len(recs), "ok": False, "status": "visibility %s: manifest only in the index repo" % vis})
            if url == clean_url(C["index_repo"]):
                log("index repo is not private: no index written")
            continue
        body = "".join(json.dumps(manifest_row(l), sort_keys=True) + "\n" for l in recs)
        if not a.apply:
            log("would write %d rows to %s %s" % (len(recs), url, fname)); out.append({"url": url, "rows": len(recs)}); continue
        ok, st = commit_file_to_branch(push_url, "backup-manifests", {fname: body, "backup-manifests/README.md": README},
                                       "backup manifest for %s %s (%d entries)" % (C["host"], C["date"], len(recs)))
        out.append({"url": url, "rows": len(recs), "ok": ok, "status": st})
        ledger({"kind": "manifest", "url": url, "rows": len(recs), "ok": ok, "status": st})
        log("%s %s: %s" % ("OK " if ok else "ERR", url, st))
    jdump(out, "manifests-%s.json" % ("apply" if a.apply else "dry"))
    print("manifests: %d repos (%s)" % (len(out), "applied" if a.apply else "dry run"))


def commit_file_to_branch(url, branch, files, msg):
    """add/replace files on `branch` without a checkout (plumbing in a temp bare repo); fast-forward only, no force."""
    tmp = tempfile.mkdtemp(prefix="pdo-man-")
    try:
        git(tmp, "init", "-q", "--bare")
        rc, o, _ = git(tmp, "ls-remote", url, "refs/heads/" + branch)
        if rc != 0:
            return False, "ls-remote failed"
        parent = o.split()[0] if o.strip() else None
        entries = {}
        if parent:
            if git(tmp, "fetch", "-q", "--depth=1", url, "refs/heads/%s:refs/heads/%s" % (branch, branch), t=600)[0] != 0:
                return False, "fetch failed"
            rc, o, _ = git(tmp, "ls-tree", "-r", parent)
            for l in o.splitlines():
                meta, path = l.split("\t", 1)
                entries[path] = meta
        for path, content in files.items():
            rc, h, _ = git(tmp, "hash-object", "-w", "--stdin", inp=content.encode())
            entries[path] = "100644 blob %s" % h.strip()
        env = dict(GENV_LOCAL, GIT_INDEX_FILE=os.path.join(tmp, "idx"))
        info = "".join("%s %s\t%s\n" % (m.split()[0], m.split()[2], p) for p, m in entries.items())
        if git(tmp, "update-index", "--index-info", env=env, inp=info.encode())[0] != 0:
            return False, "update-index failed"
        tree = git(tmp, "write-tree", env=env)[1].strip()
        rc, sha, _ = git(tmp, "commit-tree", tree, "-m", msg, *(["-p", parent] if parent else []), env=CENV)
        sha = sha.strip()
        rc, _, err = git(tmp, "push", "-q", "--no-verify", url, "%s:refs/heads/%s" % (sha, branch), t=600)
        if rc != 0:
            return False, err.strip()[-200:]
        return True, "%s@%s" % (branch, sha[:10])
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ---------------------------------------------------------------- wipe
def valid_target(u):
    home = C["home"]
    parents = [home] + [os.path.join(home, c) for c in C["containers"]]
    if os.path.realpath(u) != u or os.path.islink(u):
        return "not a canonical path"
    if os.path.dirname(u) not in parents:
        return "not a direct child of home or a container"
    b = os.path.basename(u)
    if b.startswith(".") or (os.path.dirname(u) == home and b in C["tool_dirs"]):
        return "dot or tool folder"
    if u in parents or u == "/" or len(u) <= len(home):
        return "refusing a root folder"
    if kept(u) or protected_name(u):
        return "keep/protected"
    return ""


def is_object_store(d):
    """git object dirs: content-addressed and immutable, and their inodes are often hardlinked with other clones
    (a delete elsewhere bumps their ctime). New work always shows up in refs, index or logs, which are checked."""
    return os.path.basename(d) == "objects" and (os.path.isfile(os.path.join(os.path.dirname(d), "HEAD")))


def newest_ctime(p):
    newest = 0.0
    dev = os.lstat(p).st_dev
    for dp, dn, fn in os.walk(p):
        dn[:] = [d for d in dn if not is_object_store(os.path.join(dp, d))]
        for x in dn + fn:
            try:
                st = os.lstat(os.path.join(dp, x))
            except OSError:
                continue
            newest = max(newest, st.st_ctime, st.st_mtime)
        keepd = []
        for d in dn:
            try:
                if os.lstat(os.path.join(dp, d)).st_dev == dev:
                    keepd.append(d)
            except OSError:
                pass
        dn[:] = keepd
    return newest


def safe_rmtree(top):
    """delete a tree without following symlinks or entering another filesystem: collect top-down (pruning other
    devices), then delete bottom-up. Returns (ok, skipped)."""
    dev = os.lstat(top).st_dev
    dirs, files, skipped = [top], [], []
    for dp, dn, fn in os.walk(top, topdown=True):
        keepd = []
        for d in dn:
            p = os.path.join(dp, d)
            try:
                st = os.lstat(p)
            except OSError:
                skipped.append(p); continue
            if stat.S_ISLNK(st.st_mode):
                files.append(p)
            elif st.st_dev != dev:
                skipped.append(p)
            else:
                keepd.append(d); dirs.append(p)
        dn[:] = keepd
        files += [os.path.join(dp, f) for f in fn]
    if skipped:
        return False, skipped  # something else is mounted or unreadable inside: delete nothing
    for p in files:
        try:
            os.unlink(p)
        except PermissionError:
            try:
                os.chmod(os.path.dirname(p), 0o700); os.unlink(p)
            except OSError:
                skipped.append(p)
        except FileNotFoundError:
            pass
        except OSError:
            skipped.append(p)
    for p in reversed(dirs):
        try:
            os.rmdir(p)
        except OSError:
            try:
                os.chmod(p, 0o700); os.rmdir(p)
            except OSError:
                skipped.append(p)
    return not os.path.lexists(top), skipped


def cmd_wipe(a):
    inv = jload("inventory.json")
    plan = jload("plan.json")
    led = read_ledger()
    residue = {}
    for l in led:
        if l.get("kind") == "residue":
            residue[l.get("unit")] = l  # last one wins
    exp = jload("expected-groups.json", {})
    okg = {l.get("group") for l in led if l.get("kind") in ("logs", "media") and l.get("ok")}
    groups_bad = [g for g in exp.get("groups", []) if g not in okg]
    fresh = a.apply or a.recheck
    if fresh:
        in_use, svc, _ = live_paths()  # raises ProbeError: nothing is deleted when a probe fails
    else:
        in_use, svc = set(inv["in_use"]), set(inv["service_refs"])
    rows = []
    for p in plan:
        if p["action"] != "secure-then-wipe":
            continue
        u = p["unit"]
        why = []
        res = residue.get(rel(u))
        bad = [g for g in groups_bad if g == rel(u) or g.startswith(rel(u) + "/")]
        if bad:
            why.append("log/media archive missing or failed: " + ", ".join(bad[:3]))
        if not res:
            why.append("residue backup missing")
        elif not res.get("ok"):
            why.append("residue failed: " + res.get("why", "?"))
        elif NOW - res.get("started", 0) > C["max_backup_age_hours"] * 3600:
            why.append("residue older than %dh" % C["max_backup_age_hours"])
        if not os.path.exists(u):
            why.append("already gone")
        else:
            v = valid_target(u)
            if v:
                why.append(v)
            if touches(u, in_use):
                why.append("now in use")
            if touches(u, svc):
                why.append("now referenced by a service")
            if mounts_under(u):
                why.append("mount point inside")
            if fresh and res and res.get("ok"):
                nc = newest_ctime(u)
                if nc > res.get("started", 0):
                    why.append("changed after its residue backup")
        rows.append({"unit": u, "bytes": p["bytes"], "delete": not why, "why": why})
    mode = "apply" if a.apply else "dry"
    jdump(rows, "wipe-%s.json" % mode)
    with open(os.path.join(C["state"], "wipe-%s.tsv" % mode), "w") as f:
        for r in rows:
            f.write("%s\t%.3f\t%s\t%s\n" % ("DELETE" if r["delete"] else "KEEP", r["bytes"] / 2**30, rel(r["unit"]), "; ".join(r["why"])))
    G = 2**30
    dl = [r for r in rows if r["delete"]]
    print("wipe %s: %d units, %.1f GiB eligible; %d held back" % ("APPLY" if a.apply else "dry run", len(dl),
                                                                 sum(r["bytes"] for r in dl) / G, len(rows) - len(dl)))
    if not a.apply:
        print("to apply: wipe --apply --approve %s" % sha_file(os.path.join(C["state"], "plan.json")))
        return
    if a.approve != sha_file(os.path.join(C["state"], "plan.json")):
        sys.exit("wipe --apply needs --approve <sha256 of plan.json> (printed by the dry run)")
    qroot = os.path.join(C["home"], ".prunedisk-ops", "quarantine", C["date"])
    os.makedirs(qroot, mode=0o700, exist_ok=True)
    hdev = os.lstat(C["home"]).st_dev
    repos = inv["repos"]
    me = os.getuid()
    for r in dl:
        u = r["unit"]
        live = proc_paths_fast()
        if touches(u, live) or mounts_under(u) or os.lstat(u).st_dev != hdev or os.lstat(u).st_uid != me:
            ledger({"kind": "delete", "unit": rel(u), "ok": False, "why": "in use / mount / other filesystem or owner at delete time"})
            log("HOLD", rel(u)); continue
        q = os.path.join(qroot, keyname(u))
        before = disk()["free"]
        try:
            os.rename(u, q)
        except OSError as e:
            ledger({"kind": "delete", "unit": rel(u), "ok": False, "why": "rename failed: %s" % e}); continue
        stores = {rr["common_dir"] for rr in repos if rr.get("unit") == u and rr.get("linked") and rr.get("common_dir")
                  and not inside(rr["common_dir"], u)}
        res_started = (residue.get(rel(u)) or {}).get("started", 0)
        changed = False
        for dp, dn, fn in os.walk(q):  # re-check after the rename (the top folder's own ctime changed with it)
            dn[:] = [d for d in dn if not is_object_store(os.path.join(dp, d))]
            for x in dn + fn:
                try:
                    st = os.lstat(os.path.join(dp, x))
                except OSError:
                    continue
                if max(st.st_ctime, st.st_mtime) > res_started:
                    changed = os.path.join(dp, x); break
            if changed:
                break
        if changed or touches(q, proc_paths_fast()):
            os.rename(q, u)
            ledger({"kind": "delete", "unit": rel(u), "ok": False, "why": "changed or in use after the rename: put back", "changed": changed})
            log("PUT BACK", rel(u)); continue
        ok, skipped = safe_rmtree(q)
        for cd in stores:  # remove only the admin dirs of worktrees that lived in this unit
            for gd in glob.glob(os.path.join(cd, "worktrees", "*", "gitdir")):
                try:
                    with open(gd) as fh:
                        target = fh.read().strip()
                except OSError:
                    continue
                if inside(os.path.normpath(target), u):
                    shutil.rmtree(os.path.dirname(gd), ignore_errors=True)
        ledger({"kind": "delete", "unit": rel(u), "bytes": r["bytes"], "ok": ok, "skipped": skipped[:20],
                "freed": disk()["free"] - before, "residue_key": (residue.get(rel(u)) or {}).get("dest")})
        with open(os.path.join(C["state"], "deletions.log"), "a") as f:
            f.write("%s\t%s\t%d\t%s\t%s\n" % (time.strftime("%Y-%m-%dT%H:%M:%S%z"), "deleted" if ok else "partial(quarantine)",
                                              r["bytes"], u, (residue.get(rel(u)) or {}).get("dest", "")))
        log("%s %7.2f GiB %s" % ("deleted" if ok else "PARTIAL", r["bytes"] / G, rel(u)))


# ---------------------------------------------------------------- main
def main(argv=None):
    global NET
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", help="JSON config file")
    ap.add_argument("--state", help="state folder (default ~/.prunedisk-ops/<date>)")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("inventory")
    sub.add_parser("plan")
    s = sub.add_parser("recover"); s.add_argument("--apply", action="store_true"); s.add_argument("--include-kept", action="store_true")
    s = sub.add_parser("backup"); s.add_argument("--apply", action="store_true"); s.add_argument("--max-gb", type=float, default=200)
    s = sub.add_parser("backup-dir"); s.add_argument("path"); s.add_argument("--apply", action="store_true")
    s = sub.add_parser("manifests"); s.add_argument("--apply", action="store_true")
    s = sub.add_parser("wipe"); s.add_argument("--apply", action="store_true"); s.add_argument("--recheck", action="store_true")
    s.add_argument("--approve", default="", help="sha256 of plan.json, printed by the dry run")
    a = ap.parse_args(argv)
    global NOW
    NOW = time.time()
    C.clear(); C.update(json.loads(json.dumps(DEFAULTS)))
    C["date"] = time.strftime("%Y%m%d")
    if a.config:
        with open(a.config) as f:
            C.update(json.load(f))
    C["home"] = os.path.realpath(os.path.expanduser(C["home"]))
    LSR.clear(); VIS.clear()
    del GSAFE[8:]
    if C.get("credential_helper"):
        GSAFE.extend(["-c", "credential.helper=", "-c", "credential.helper=" + C["credential_helper"]])
    NET = re.compile(r"^(https?://|ssh://|git@|file://|/)") if C.get("allow_file_remotes") else re.compile(r"^(https?://|ssh://|git@)")
    C["state"] = a.state or os.path.join(C["home"], ".prunedisk-ops", C["date"])
    os.makedirs(C["state"], mode=0o700, exist_ok=True)
    if a.cmd in ("backup", "backup-dir") and getattr(a, "apply", False) and not C["remote"]:
        sys.exit("config needs `remote` (an rclone destination) for backup --apply")
    try:
        {"inventory": cmd_inventory, "plan": cmd_plan, "recover": cmd_recover, "backup": cmd_backup, "backup-dir": cmd_backup_dir,
         "manifests": cmd_manifests, "wipe": cmd_wipe}[a.cmd](a)
    except ProbeError as e:
        sys.exit("ABORT: liveness probe failed (%s); nothing was changed by this step" % e)


if __name__ == "__main__":
    main()
