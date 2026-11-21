import json, os, subprocess, sys, tempfile, time, unittest, io, contextlib
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import prunedisk_ops as pdo

OLD = time.time() - 5 * 86400


def sh(*a, cwd=None):
    subprocess.run(a, cwd=cwd, check=True, capture_output=True,
                   env=dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t"))


def fresh(path):
    """tests run with a 2-day clock offset (ctime cannot be faked), so 'just touched' means mtime = now + 2 days."""
    t = time.time() + 2 * 86400
    for dp, dn, fn in os.walk(path):
        for f in fn + dn:
            os.utime(os.path.join(dp, f), (t, t), follow_symlinks=False)


def age(path):
    for dp, dn, fn in os.walk(path):
        for f in fn + dn:
            try:
                os.utime(os.path.join(dp, f), (OLD, OLD), follow_symlinks=False)
            except OSError:
                pass


class PDO(unittest.TestCase):
    def setUp(self):
        self._live = pdo.live_paths
        pdo.live_paths = lambda: (set(), set(), {"stub": True})
        self.addCleanup(setattr, pdo, "live_paths", self._live)
        self.t = tempfile.mkdtemp()
        self.home = os.path.join(self.t, "home"); os.makedirs(self.home)
        self.store = os.path.join(self.t, "store"); os.makedirs(self.store)
        self.origin = os.path.join(self.t, "origin.git")
        self.salvage = os.path.join(self.t, "salvage.git")
        sh("git", "init", "-q", "-b", "main", "--bare", self.origin); sh("git", "init", "-q", "-b", "main", "--bare", self.salvage)
        seed = os.path.join(self.t, "seed"); sh("git", "init", "-q", "-b", "main", seed)
        open(os.path.join(seed, "a.txt"), "w").write("a\n"); sh("git", "add", ".", cwd=seed); sh("git", "commit", "-qm", "a", cwd=seed)
        sh("git", "push", "-q", self.origin, "main", cwd=seed)
        self.cfg = os.path.join(self.t, "cfg.json")
        json.dump({"home": self.home, "host": "testhost", "date": "20990101", "remote": self.store, "allow_file_remotes": True,
                   "salvage_repo": self.salvage, "index_repo": self.salvage, "protect_names": ["clientx"], "keep": ["keepme"],
                   "containers": ["work"], "visibility_default": "PRIVATE",
                   "test_clock_offset_s": 2 * 86400}, open(self.cfg, "w"))

    def clone(self, name):
        p = os.path.join(self.home, name)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        sh("git", "clone", "-q", self.origin, p)
        return p

    def pdo(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            pdo.main(["--config", self.cfg, "--state", os.path.join(self.t, "state")] + list(args))
        return out.getvalue()

    def state(self, name):
        return json.load(open(os.path.join(self.t, "state", name)))

    def ledger(self):
        p = os.path.join(self.t, "state", "ledger.jsonl")
        return [json.loads(l) for l in open(p)] if os.path.exists(p) else []

    def build(self):
        clean = self.clone("work/clean")
        ahead = self.clone("work/ahead")
        open(os.path.join(ahead, "b.txt"), "w").write("b\n"); sh("git", "add", ".", cwd=ahead); sh("git", "commit", "-qm", "b", cwd=ahead)
        dirty = self.clone("work/dirty")
        open(os.path.join(dirty, "a.txt"), "a").write("change\n")
        open(os.path.join(dirty, "new.py"), "w").write("print(1)\n")
        open(os.path.join(dirty, ".env"), "w").write("TOKEN=abc\n")
        os.makedirs(os.path.join(dirty, "node_modules", "x")); open(os.path.join(dirty, "node_modules", "x", "i.js"), "w").write("x")
        open(os.path.join(dirty, "run.log"), "w").write("log line\n")
        active = self.clone("work/active"); fresh(active)
        prot = self.clone("work/clientx-app")
        keep = self.clone("keepme")
        loose = os.path.join(self.home, "loose"); os.makedirs(loose); open(os.path.join(loose, "notes.md"), "w").write("n\n")
        for p in (clean, ahead, dirty, prot, keep, loose):
            age(p)
        return dict(clean=clean, ahead=ahead, dirty=dirty, active=active, prot=prot, keep=keep, loose=loose)

    def test_full_cycle(self):
        P = self.build()
        self.pdo("inventory")
        inv = self.state("inventory.json")
        self.assertEqual(len([r for r in inv["repos"]]), 6)
        self.pdo("plan")
        plan = {os.path.basename(p["unit"]): p for p in self.state("plan.json")}
        self.assertEqual(plan["active"]["action"], "keep")
        self.assertEqual(plan["clientx-app"]["action"], "keep")
        self.assertEqual(plan["keepme"]["action"], "keep")
        for k in ("clean", "ahead", "dirty", "loose"):
            self.assertEqual(plan[k]["action"], "secure-then-wipe", k)
        # dry runs change nothing
        self.pdo("recover"); self.pdo("backup"); self.pdo("wipe")
        self.assertEqual(self.ledger(), [])
        self.assertTrue(all(os.path.exists(p) for p in P.values()))
        self.pdo("recover", "--apply")
        rec = [l for l in self.ledger() if l["kind"] == "recovery"]
        self.assertTrue(all(l["ok"] for l in rec))
        heads = subprocess.run(["git", "ls-remote", self.origin], capture_output=True, text=True).stdout
        self.assertIn("recovery/testhost-20990101-work-ahead-head-", heads)
        self.assertIn("recovery/testhost-20990101-work-dirty-wip-", heads)
        self.assertNotIn("recovery/testhost-20990101-work-clean", heads)
        wip = [l["sha"] for l in rec if "-wip-" in l["branch"]][0]
        files = subprocess.run(["git", "--git-dir", self.origin, "ls-tree", "-r", "--name-only", wip], capture_output=True, text=True).stdout.split()
        self.assertIn("new.py", files); self.assertNotIn(".env", files); self.assertNotIn("run.log", files)
        # working tree / refs of the dirty repo untouched
        st = subprocess.run(["git", "-C", P["dirty"], "status", "--porcelain"], capture_output=True, text=True,
                            env=dict(os.environ, GIT_OPTIONAL_LOCKS="0")).stdout
        self.assertIn(" M a.txt", st)
        self.pdo("backup", "--apply")
        led = self.ledger()
        res = {l["unit"]: l for l in led if l["kind"] == "residue"}
        self.assertTrue(res["work/dirty"]["ok"])
        self.assertNotIn("work/dirty/.env", res["work/dirty"]["paths"])      # secrets never uploaded
        self.assertTrue(os.path.exists(os.path.join(self.t, "state", "vault", "work", "dirty", ".env")))
        self.assertIn("work/dirty/a.txt", res["work/dirty"]["paths"])      # modified tracked file
        self.assertNotIn("work/clean/a.txt", res.get("work/clean", {}).get("paths", []))  # recoverable from git
        self.assertFalse(any("node_modules" in p for p in res["work/dirty"]["paths"]))
        logs = [l for l in led if l["kind"] == "logs"]
        self.assertTrue(any("work/dirty/run.log" in l["paths"] for l in logs))
        out = self.pdo("wipe")
        rows = {os.path.basename(r["unit"]): r for r in self.state("wipe-dry.json")}
        self.assertTrue(rows["dirty"]["delete"], rows["dirty"])
        # touch one unit after the plan: apply must hold it back
        open(os.path.join(P["clean"], "fresh.txt"), "w").write("new"); fresh(os.path.join(P["clean"], "fresh.txt"))
        self.wipe_apply()
        self.assertFalse(os.path.exists(P["dirty"]), open(os.path.join(self.t, "state", "wipe-apply.tsv")).read()); self.assertFalse(os.path.exists(P["ahead"])); self.assertFalse(os.path.exists(P["loose"]))
        self.assertTrue(os.path.exists(P["clean"])); self.assertTrue(os.path.exists(P["active"]))
        self.assertTrue(os.path.exists(P["prot"])); self.assertTrue(os.path.exists(P["keep"]))
        dl = open(os.path.join(self.t, "state", "deletions.log")).read()
        self.assertIn("work/dirty", dl)
        self.pdo("manifests", "--apply")
        man = subprocess.run(["git", "--git-dir", self.origin, "show", "backup-manifests:backup-manifests/testhost-20990101.jsonl"],
                             capture_output=True, text=True)
        self.assertEqual(man.returncode, 0, man.stderr)
        self.assertIn("recovery", man.stdout)
        idx = subprocess.run(["git", "--git-dir", self.salvage, "show", "backup-manifests:backup-manifests/testhost-20990101.jsonl"],
                             capture_output=True, text=True)
        self.assertEqual(idx.returncode, 0); self.assertIn("residue", idx.stdout)

    def test_missing_residue_holds_wipe(self):
        P = self.build()
        self.pdo("inventory"); self.pdo("plan"); self.pdo("recover", "--apply")
        self.wipe_apply()
        self.assertTrue(all(os.path.exists(p) for p in P.values()))

    def test_service_reference_keeps_unit(self):
        P = self.build()
        pdo.live_paths = lambda: (set(), {P["loose"] + "/notes.md"}, {})
        self.pdo("inventory"); self.pdo("plan")
        plan = {os.path.basename(p["unit"]): p for p in self.state("plan.json")}
        self.assertEqual(plan["loose"]["action"], "keep")
        self.assertIn("referenced by a service, cron job or container", plan["loose"]["why"])

    def test_worktree_object_store_kept(self):
        base = self.clone("work/base"); age(base)
        wt = os.path.join(self.home, "work", "wt1")
        sh("git", "worktree", "add", "-q", "-b", "feat", wt, cwd=base)  # fresh => active => keep; base must be kept too
        fresh(wt)
        self.pdo("inventory"); self.pdo("plan")
        plan = {os.path.basename(p["unit"]): p for p in self.state("plan.json")}
        self.assertEqual(plan["wt1"]["action"], "keep")
        self.assertEqual(plan["base"]["action"], "keep", plan["base"])

    def test_noise_repo_not_pushed(self):
        p = os.path.join(self.home, "work", "u1", ".scratch", "fx"); os.makedirs(p)
        sh("git", "init", "-q", p); open(os.path.join(p, "f"), "w").write("1"); sh("git", "add", ".", cwd=p); sh("git", "commit", "-qm", "x", cwd=p)
        age(os.path.join(self.home, "work", "u1"))
        self.pdo("inventory"); self.pdo("plan"); self.pdo("recover", "--apply")
        self.assertEqual(subprocess.run(["git", "ls-remote", self.salvage], capture_output=True, text=True).stdout.strip(), "")
        self.pdo("backup", "--apply")
        res = [l for l in self.ledger() if l["kind"] == "residue"][0]
        self.assertTrue(any(x.endswith("fx/.git/HEAD") for x in res["paths"]))  # whole repo archived

    def wipe_apply(self):
        sha = pdo.sha_file(os.path.join(self.t, "state", "plan.json"))
        return self.pdo("wipe", "--apply", "--approve", sha)

    def test_wipe_needs_approval(self):
        P = self.build()
        self.cycle_until_backup()
        with self.assertRaises(SystemExit):
            self.pdo("wipe", "--apply", "--approve", "0" * 64)
        self.assertTrue(all(os.path.exists(p) for p in P.values()))

    def test_commit_after_recover_is_archived(self):
        P = self.build()
        self.pdo("inventory"); self.pdo("plan"); self.pdo("recover", "--apply")
        with open(os.path.join(P["clean"], "late.txt"), "w") as f:
            f.write("late\n")
        sh("git", "add", ".", cwd=P["clean"]); sh("git", "commit", "-qm", "late", cwd=P["clean"])
        self.pdo("backup", "--apply")
        res = {l["unit"]: l for l in self.ledger() if l["kind"] == "residue"}
        self.assertTrue(any("work/clean/.git/objects" in x for x in res["work/clean"]["paths"]), res["work/clean"]["paths"])

    def test_big_blob_not_pushed(self):
        p = self.clone("work/big")
        with open(os.path.join(p, "blob.bin"), "wb") as f:
            f.write(os.urandom(9 << 20))
        sh("git", "add", ".", cwd=p); sh("git", "commit", "-qm", "big", cwd=p); age(p)
        self.cycle_until_backup()
        heads = subprocess.run(["git", "ls-remote", self.origin], capture_output=True, text=True).stdout
        self.assertNotIn("work-big", heads)

    def test_rsync_style_copy_is_active(self):
        p = os.path.join(self.home, "work", "copied"); os.makedirs(p)
        with open(os.path.join(p, "f"), "w") as f:
            f.write("x")
        age(p)  # old mtime, fresh ctime: like rsync -a
        cfg = json.load(open(self.cfg)); cfg["test_clock_offset_s"] = 0; json.dump(cfg, open(self.cfg, "w"))
        self.pdo("inventory"); self.pdo("plan")
        plan = {os.path.basename(x["unit"]): x for x in self.state("plan.json")}
        self.assertEqual(plan["copied"]["action"], "keep")

    def test_zip_never_in_snapshot(self):
        p = self.clone("work/zippy")
        import zipfile
        with zipfile.ZipFile(os.path.join(p, "handoff.zip"), "w") as z:
            z.writestr(".env", "TOKEN=x")
        with open(os.path.join(p, "note.md"), "w") as f:
            f.write("n\n")
        age(p)
        self.cycle_until_backup()
        wip = [l for l in self.ledger() if l["kind"] == "recovery" and "zippy" in l["branch"]][0]["sha"]
        files = subprocess.run(["git", "--git-dir", self.origin, "ls-tree", "-r", "--name-only", wip], capture_output=True, text=True).stdout.split()
        self.assertIn("note.md", files); self.assertNotIn("handoff.zip", files)
        res = {l["unit"]: l for l in self.ledger() if l["kind"] == "residue"}
        self.assertIn("work/zippy/handoff.zip", res["work/zippy"]["paths"])

    def cycle_until_backup(self):
        self.pdo("inventory"); self.pdo("plan"); self.pdo("recover", "--apply"); self.pdo("backup", "--apply")

    def test_probe_failure_aborts_wipe(self):
        P = self.build()
        self.cycle_until_backup()
        def boom():
            raise pdo.ProbeError("docker ps failed")
        pdo.live_paths = boom
        with self.assertRaises(SystemExit):
            self.wipe_apply()
        self.assertTrue(all(os.path.exists(p) for p in P.values()))

    def test_public_target_not_pushed(self):
        P = self.build()
        cfg = json.load(open(self.cfg)); cfg["visibility_default"] = "PUBLIC"; json.dump(cfg, open(self.cfg, "w"))
        self.cycle_until_backup()
        heads = subprocess.run(["git", "ls-remote", self.origin], capture_output=True, text=True).stdout
        self.assertNotIn("recovery/", heads)
        res = {l["unit"]: l for l in self.ledger() if l["kind"] == "residue"}
        self.assertTrue(any("work/ahead/.git/objects" in p for p in res["work/ahead"]["paths"]), res["work/ahead"]["paths"])

    def test_secret_in_history_blocks_push(self):
        p = self.clone("work/leaky")
        open(os.path.join(p, "cfg.py"), "w").write("KEY='ghp_" + "a" * 36 + "'\n")
        sh("git", "add", ".", cwd=p); sh("git", "commit", "-qm", "oops", cwd=p); age(p)
        self.cycle_until_backup()
        heads = subprocess.run(["git", "ls-remote", self.origin], capture_output=True, text=True).stdout
        self.assertNotIn("recovery/", heads)
        self.assertTrue(any(l["kind"] == "recovery-skip" for l in self.ledger()))
        res = {l["unit"]: l for l in self.ledger() if l["kind"] == "residue"}
        self.assertTrue(res["work/leaky"]["ok"])
        self.assertTrue(any("/.git/objects" in x for x in res["work/leaky"]["paths"]))

    def test_stale_tracking_ref_not_trusted(self):
        p = self.clone("work/stale")
        sh("git", "checkout", "-qb", "feat", cwd=p)
        open(os.path.join(p, "f.txt"), "w").write("f\n"); sh("git", "add", ".", cwd=p); sh("git", "commit", "-qm", "f", cwd=p)
        sh("git", "push", "-q", "origin", "feat", cwd=p)
        sh("git", "--git-dir", self.origin, "branch", "-D", "feat")  # gone on the remote; local tracking ref is stale
        age(p)
        self.pdo("inventory")
        r = [r for r in self.state("inventory.json")["repos"] if r["path"].endswith("work/stale")][0]
        self.assertTrue(r["unpushed"], r)

    def test_alternates_borrower_keeps_unit(self):
        base = self.clone("work/objsrc"); age(base)
        borrower = os.path.join(self.home, "keepme2")
        sh("git", "clone", "-q", "--shared", base, borrower)
        cfg = json.load(open(self.cfg)); cfg["keep"].append("keepme2"); json.dump(cfg, open(self.cfg, "w"))
        self.pdo("inventory"); self.pdo("plan")
        plan = {os.path.basename(p["unit"]): p for p in self.state("plan.json")}
        self.assertEqual(plan["objsrc"]["action"], "keep", plan["objsrc"])

    def test_tracked_regen_dir_is_backed_up(self):
        p = os.path.join(self.home, "work", "vend"); os.makedirs(os.path.join(p, "node_modules", "patched"))
        sh("git", "init", "-q", "-b", "main", p)
        open(os.path.join(p, "node_modules", "patched", "i.js"), "w").write("x")
        sh("git", "add", "-f", ".", cwd=p); sh("git", "commit", "-qm", "v", cwd=p); age(p)
        self.cycle_until_backup()
        res = {l["unit"]: l for l in self.ledger() if l["kind"] == "residue"}
        self.assertTrue(any("node_modules/patched/i.js" in x for x in res["work/vend"]["paths"]))


if __name__ == "__main__":
    unittest.main()
