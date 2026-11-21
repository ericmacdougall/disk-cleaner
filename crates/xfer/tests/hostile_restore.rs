//! AC-15 and NEG-5: restore treats the bucket as untrusted input. Each hostile repository here is
//! built by hand and is self-consistent (packs sit at their content address and are listed with
//! the checksum their index records), so the refusal comes from the check under test and not from
//! an earlier address or checksum mismatch. A control repository from the same builder restores
//! cleanly. Every restore runs the CLI as a child process, so its peak RSS is its own.
#![cfg(unix)]

mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use xfer::hash::{ChunkId, HashKind};
use xfer::meta::{Identity, Kind};
use xfer::pack::{pack_key, OpenSegment, PackBuilder, SegmentInfo, Trailer, CODEC_RAW, CODEC_ZSTD};
use xfer::repo::{ChunkRef, Entry, FilesObj, IdxObj, ObjRef, FORMAT_BYTES};

const BIN: &str = env!("CARGO_BIN_EXE_prunedisk-xfer");
const RSS_MAX: u64 = 256 << 20;
const PARENT_MAX: u64 = 96 << 20;

fn rss_unit() -> u64 {
    if cfg!(target_os = "macos") {
        1
    } else {
        1024
    }
}

/// This process's own peak RSS: what a child spawned from it starts its `ru_maxrss` at.
fn self_rss() -> u64 {
    // On Linux, RUSAGE_SELF also carries the peak of whatever process spawned this one (cargo,
    // over 100 MiB on CI), because exec keeps the old peak. The child inherits only this
    // address space's peak, which is VmHWM, so read that.
    if cfg!(target_os = "linux") {
        let s = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
        let kb: u64 = s
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .expect("VmHWM");
        return kb << 10;
    }
    // SAFETY: zeroed rusage is a valid value for getrusage to fill.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_maxrss as u64 * rss_unit()
}
const RUN: &str = "r0000000000ab-hostile1";

fn put(env: &Env, key: &str, bytes: &[u8]) {
    write(&env.p("store").join(key), bytes);
}

fn objref(key: &str, bytes: &[u8]) -> ObjRef {
    ObjRef {
        key: key.to_string(),
        size: bytes.len() as u64,
        hash: HashKind::Sha1.digest(bytes),
    }
}

fn zjson<T: serde::Serialize>(v: &T) -> Vec<u8> {
    zstd::bulk::compress(&serde_json::to_vec(v).expect("json"), 3).expect("zstd")
}

fn entry(path: &[u8], kind: Kind, mode: u32, data: &[u8]) -> Entry {
    let (p, ph) = xfer::receipt::path_fields(path);
    let file = kind == Kind::File;
    Entry {
        path: p,
        path_hex: ph,
        kind,
        mode,
        mtime_ns: 1_600_000_000_000_000_000,
        size: if file { data.len() as u64 } else { 0 },
        id: Identity {
            dev: 1,
            ino: 1,
            size: data.len() as u64,
            mtime_ns: 1_600_000_000_000_000_000,
            ctime_ns: 1_600_000_000_000_000_000,
            nlink: 1,
        },
        blake3: file.then(|| blake3::hash(data).to_hex().to_string()),
        chunks: if file && !data.is_empty() {
            vec![ChunkId::of(data)]
        } else {
            Vec::new()
        },
        target: None,
        target_hex: None,
        xattrs: Vec::new(),
        flags: 0,
        alloc: 4096,
        group: None,
        meta_incomplete: false,
    }
}

fn link(path: &str, kind: Kind, target: &[u8]) -> Entry {
    let mut e = entry(path.as_bytes(), kind, 0o777, b"");
    let (t, th) = xfer::receipt::path_fields(target);
    e.target = t;
    e.target_hex = th;
    e
}

/// A valid pack holding `chunks` in one raw segment.
fn pack_of(chunks: &[&[u8]]) -> Vec<u8> {
    let mut seg = OpenSegment::default();
    for c in chunks {
        seg.push(ChunkId::of(c), c);
    }
    let mut pb = PackBuilder::new();
    let raw = seg.data.clone();
    pb.add_segment(&seg, CODEC_RAW, &raw);
    pb.finish().expect("pack").0
}

/// A pack with a hand-written trailer (any segments and chunk locations) after `body`.
fn pack_with(body: &[u8], t: &Trailer) -> Vec<u8> {
    let mut b = xfer::pack::HEAD.to_vec();
    b.extend_from_slice(body);
    let z = zjson(t);
    b.extend_from_slice(&z);
    b.extend_from_slice(&(z.len() as u32).to_le_bytes());
    b.extend_from_slice(xfer::pack::TAIL);
    b
}

/// One run: `pack` at its content address, an index that lists it with `chunks`, and a file table
/// of `entries`. Index and file-table bytes can be replaced to test those decoders.
struct Repo {
    pack: Vec<u8>,
    chunks: Vec<ChunkRef>,
    entries: Vec<Entry>,
    idx_bytes: Option<Vec<u8>>,
    files_bytes: Option<Vec<u8>>,
}

impl Repo {
    fn new(datas: &[&[u8]], entries: Vec<Entry>) -> Repo {
        Repo {
            pack: pack_of(datas),
            chunks: datas
                .iter()
                .map(|d| ChunkRef {
                    id: ChunkId::of(d),
                    p: 0,
                    len: d.len() as u32,
                })
                .collect(),
            entries,
            idx_bytes: None,
            files_bytes: None,
        }
    }

    fn write(&self, env: &Env) {
        put(env, "v1/format.json", FORMAT_BYTES);
        let pk = pack_key(&self.pack);
        put(env, &pk, &self.pack);
        let idx = IdxObj {
            v: 1,
            engine: "disk-cleaner-xfer 0.1.0+unknown".into(),
            run: RUN.into(),
            seq: 0,
            root: "src".into(),
            hash_kind: HashKind::Sha1,
            packs: vec![objref(&pk, &self.pack)],
            chunks: self.chunks.clone(),
        };
        let files = FilesObj {
            v: 1,
            engine: idx.engine.clone(),
            run: RUN.into(),
            seq: 0,
            root: "src".into(),
            entries: self.entries.clone(),
        };
        let ib = self.idx_bytes.clone().unwrap_or_else(|| zjson(&idx));
        let fb = self.files_bytes.clone().unwrap_or_else(|| zjson(&files));
        put(env, &format!("v1/index/{RUN}/000000.idx"), &ib);
        put(env, &format!("v1/files/{RUN}/000000.files"), &fb);
    }
}

struct Out {
    code: i32,
    text: String,
    rss: u64,
}

/// Run the CLI restore as a child and take its peak RSS from wait4 (which reaps it, so the
/// `Child` handle is never waited on again).
#[allow(clippy::zombie_processes)]
fn restore(env: &Env, target: &Path, extra: &[&str]) -> Out {
    let (o, e) = (env.p("stdout.txt"), env.p("stderr.txt"));
    // A child's ru_maxrss starts at its parent's RSS at spawn (Linux counts the image replaced by
    // exec), so the reading is an upper bound only while this process stays small.
    assert!(
        self_rss() < PARENT_MAX,
        "the test process holds {} MiB, too much to measure a child",
        self_rss() >> 20
    );
    let child = Command::new(BIN)
        .args(["restore", "--repo"])
        .arg(format!("local:{}", env.p("store").display()))
        .args(["--mem-budget", "640Mi", "--max-pack", "16Mi", "--run", RUN])
        .arg("--target")
        .arg(target)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&o).expect("stdout"))
        .stderr(std::fs::File::create(&e).expect("stderr"))
        .spawn()
        .expect("spawn restore");
    let mut status = 0;
    // SAFETY: zeroed rusage is a valid value; wait4 on our own child's pid, waited once.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::wait4(child.id() as libc::pid_t, &mut status, 0, &mut ru) };
    assert_eq!(r, child.id() as libc::pid_t, "wait4");
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };
    let unit = rss_unit();
    Out {
        code,
        text: std::fs::read_to_string(&o).unwrap_or_default()
            + &std::fs::read_to_string(&e).unwrap_or_default(),
        rss: ru.ru_maxrss as u64 * unit,
    }
}

/// Files under the environment outside the store and the restore target, with their bytes.
fn outside(env: &Env) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![env.dir.path().to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("read_dir").flatten() {
            let p = e.path();
            let name = p.file_name().expect("name").to_string_lossy().into_owned();
            if d == env.dir.path()
                && ["store", "target", "stdout.txt", "stderr.txt"].contains(&name.as_str())
            {
                continue;
            }
            let md = std::fs::symlink_metadata(&p).expect("lstat");
            if md.is_dir() {
                stack.push(p);
            } else {
                out.push((p.clone(), std::fs::read(&p).unwrap_or_default()));
            }
        }
    }
    out.sort();
    out
}

/// Write `r`, restore it, and check the bounds every hostile case shares: no write outside the
/// target, peak RSS under the cap. Returns the CLI result.
fn run_case(r: &Repo) -> (Env, Out) {
    let env = Env::new();
    write(&env.p("outside/keep.txt"), b"must stay untouched\n");
    r.write(&env);
    let before = outside(&env);
    let out = restore(&env, &env.p("target"), &[]);
    eprintln!("[{} MiB] {}", out.rss >> 20, out.text.trim());
    assert_eq!(
        outside(&env),
        before,
        "a write landed outside the target: {}",
        out.text
    );
    assert!(
        !env.dir.path().join("escape.txt").exists() && !Path::new("/tmp/xfer-abs.txt").exists()
    );
    assert!(
        out.rss < RSS_MAX,
        "peak RSS {} MiB: {}",
        out.rss >> 20,
        out.text
    );
    (env, out)
}

fn refused_before_target(r: &Repo, what: &str, want: &str) {
    let (env, out) = run_case(r);
    assert_eq!(out.code, 3, "{what} was not refused: {}", out.text);
    assert!(
        out.text.contains(want),
        "{what}: expected {want:?}: {}",
        out.text
    );
    assert!(!env.p("target").exists(), "{what}: the target was created");
}

#[test]
fn the_hand_built_control_repository_restores() {
    let a = rand_bytes(1, 5000);
    let r = Repo::new(
        &[&a],
        vec![
            entry(b"d", Kind::Dir, 0o755, b""),
            entry(b"d/f.bin", Kind::File, 0o640, &a),
            link("d/hard.bin", Kind::Hardlink, b"d/f.bin"),
            link("d/sym", Kind::Symlink, b"f.bin"),
        ],
    );
    let (env, out) = run_case(&r);
    assert_eq!(out.code, 0, "{}", out.text);
    assert_eq!(std::fs::read(env.p("target/d/f.bin")).expect("restored"), a);
    assert_eq!(
        std::fs::read_link(env.p("target/d/sym")).expect("link"),
        Path::new("f.bin")
    );
}

#[test]
fn unsafe_paths_and_links_are_refused_before_the_target_exists() {
    let a = rand_bytes(2, 3000);
    let file = |p: &[u8]| entry(p, Kind::File, 0o644, &a);
    let cases: Vec<(&str, Vec<Entry>)> = vec![
        ("a .. component", vec![file(b"../escape.txt")]),
        (
            "an inner ..",
            vec![
                entry(b"d", Kind::Dir, 0o755, b""),
                file(b"d/../../escape.txt"),
            ],
        ),
        ("an absolute path", vec![file(b"/tmp/xfer-abs.txt")]),
        (
            "an empty component",
            vec![entry(b"d", Kind::Dir, 0o755, b""), file(b"d//f")],
        ),
        ("a NUL in a name", vec![file(b"a\0b")]),
        ("a dot component", vec![file(b"./f")]),
        (
            "a hard link to a missing member",
            vec![link("h", Kind::Hardlink, b"nope")],
        ),
        (
            "a hard link outside the tree",
            vec![link("h", Kind::Hardlink, b"../outside/keep.txt")],
        ),
        (
            "a hard link to a directory",
            vec![
                entry(b"d", Kind::Dir, 0o755, b""),
                link("h", Kind::Hardlink, b"d"),
            ],
        ),
        (
            "a file under a symlink",
            vec![
                link("lnk", Kind::Symlink, b"../outside"),
                file(b"lnk/evil.txt"),
            ],
        ),
        (
            "a file without its directory",
            vec![file(b"missing-dir/f.bin")],
        ),
        ("a path listed twice", vec![file(b"f"), file(b"f")]),
        (
            "a symlink without a target",
            vec![entry(b"s", Kind::Symlink, 0o777, b"")],
        ),
    ];
    for (what, entries) in cases {
        let want = match what {
            w if w.starts_with("a hard link to") => "missing or non-file member",
            "a file under a symlink" | "a file without its directory" => "is not a directory",
            "a path listed twice" => "appears twice",
            "a symlink without a target" => "symlink without a target",
            _ => "unsafe relative path",
        };
        refused_before_target(&Repo::new(&[&a], entries), what, want);
    }
}

/// `n` copies of `byte` as one zstd frame, encoded in a stream so this process never holds them
/// (a child's peak RSS includes its parent's at spawn, so the parent stays small).
fn bomb_of(byte: u8, n: usize) -> Vec<u8> {
    use std::io::Write;
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).expect("encoder");
    let block = [byte; 64 << 10];
    for _ in 0..n / block.len() {
        enc.write_all(&block).expect("encode");
    }
    enc.finish().expect("finish")
}

fn zero_bomb(n: usize) -> Vec<u8> {
    bomb_of(0, n)
}

#[test]
fn hostile_commit_objects_are_refused_within_bounded_memory() {
    let a = rand_bytes(3, 3000);
    let base = || Repo::new(&[&a], vec![entry(b"f.bin", Kind::File, 0o644, &a)]);
    // an expansion bomb: 100 MiB of spaces in a few KiB, over the 64 MiB object limit. Spaces,
    // not zeros: JSON allows leading whitespace, so only the size limit can stop it (zeros are
    // refused at the first byte by a parser that reads as it decompresses).
    let bomb = bomb_of(b' ', 100 << 20);
    assert!(bomb.len() < 64 << 10);
    // a frame whose header declares about 4 GiB of content, followed by one tiny raw block
    let mut big = vec![0x28, 0xb5, 0x2f, 0xfd, 0xa0, 0xff, 0xff, 0xff, 0xff];
    let n: u32 = 4;
    big.extend_from_slice(&(1 | (n << 3)).to_le_bytes()[..3]);
    big.extend_from_slice(b"{\"v\"");
    let over = "exceeds the 67108864 byte limit";
    let frame = "Frame requires too much memory";
    for (what, idx, files, want) in [
        ("a bomb as the file table", None, Some(bomb.clone()), over),
        ("a bomb as the index", Some(bomb.clone()), None, over),
        (
            "a 4 GiB frame as the file table",
            None,
            Some(big.clone()),
            frame,
        ),
        ("a 4 GiB frame as the index", Some(big.clone()), None, frame),
        (
            "a file table that is not zstd",
            None,
            Some(b"{\"v\":1}".to_vec()),
            "zstd",
        ),
    ] {
        let mut r = base();
        r.idx_bytes = idx;
        r.files_bytes = files;
        refused_before_target(&r, what, want);
    }
}

#[test]
fn hostile_packs_are_refused_and_restore_nothing_from_them() {
    let a = rand_bytes(4, 3000);
    let id = ChunkId::of(&a);
    let good = pack_of(&[&a]);
    let n = good.len();
    let seg = |off: u64, clen: u64, ulen: u64, codec: u8| SegmentInfo {
        off,
        clen,
        ulen,
        codec,
    };
    let chunk = |off: u32, len: u32| xfer::pack::ChunkLoc {
        id,
        seg: 0,
        off,
        len,
    };
    let trailer = |segments, chunks| Trailer {
        v: 1,
        segments,
        chunks,
    };
    // 64 MiB of zeros declared as a 4 KiB segment
    let zbomb = zero_bomb(64 << 20);
    let mut cases: Vec<(&str, Vec<u8>, &str)> = Vec::new();
    let mut long = good.clone();
    long[n - 12..n - 8].copy_from_slice(&u32::MAX.to_le_bytes());
    cases.push((
        "a trailer length beyond the object",
        long,
        "trailer length out of range",
    ));
    cases.push((
        "a truncated trailer",
        {
            let mut t = good[..n - 20].to_vec();
            t.extend_from_slice(&good[n - 12..]);
            t
        },
        "zstd",
    ));
    cases.push((
        "a missing end magic",
        good[..n - 1].to_vec(),
        "pack magic missing",
    ));
    cases.push((
        "a segment offset beyond the body",
        pack_with(
            &a,
            &trailer(
                vec![seg(1 << 40, a.len() as u64, a.len() as u64, CODEC_RAW)],
                vec![chunk(0, a.len() as u32)],
            ),
        ),
        "segment out of range",
    ));
    cases.push((
        "a segment overflowing u64",
        pack_with(
            &a,
            &trailer(
                vec![seg(u64::MAX - 1, 16, 16, CODEC_RAW)],
                vec![chunk(0, 16)],
            ),
        ),
        "segment overflow",
    ));
    cases.push((
        "a chunk beyond its segment",
        pack_with(
            &a,
            &trailer(
                vec![seg(8, a.len() as u64, a.len() as u64, CODEC_RAW)],
                vec![chunk(a.len() as u32 - 10, 3000)],
            ),
        ),
        "chunk out of segment range",
    ));
    cases.push((
        "a segment declaring over 4 MiB",
        pack_with(
            &a,
            &trailer(
                vec![seg(8, a.len() as u64, 5 << 20, CODEC_ZSTD)],
                vec![chunk(0, a.len() as u32)],
            ),
        ),
        "segment out of range",
    ));
    cases.push((
        "a segment expansion bomb",
        pack_with(
            &zbomb,
            &trailer(
                vec![seg(8, zbomb.len() as u64, 4096, CODEC_ZSTD)],
                vec![chunk(0, 3000)],
            ),
        ),
        "exceeds the 4096 byte limit",
    ));
    cases.push((
        "a chunk whose bytes do not match its id",
        pack_with(
            &rand_bytes(5, 3000),
            &trailer(vec![seg(8, 3000, 3000, CODEC_RAW)], vec![chunk(0, 3000)]),
        ),
        "does not match its id",
    ));
    for (what, pack, want) in cases {
        let mut r = Repo::new(&[&a], vec![entry(b"f.bin", Kind::File, 0o644, &a)]);
        r.pack = pack;
        let pk = pack_key(&r.pack);
        let (env, out) = run_case(&r);
        assert_eq!(out.code, 3, "{what} was not refused: {}", out.text);
        // the index and file table were valid, so the refusal is the pack check's, naming it
        assert!(
            env.p("target").is_dir(),
            "{what}: refused before the pack check: {}",
            out.text
        );
        assert!(
            out.text.contains(&pk) && out.text.contains(want),
            "{what}: expected {want:?} naming {pk}: {}",
            out.text
        );
        // the file was created empty and sized, but never written from the hostile pack
        let got = std::fs::read(env.p("target/f.bin")).unwrap_or_default();
        assert_ne!(got, a, "{what}: the file was restored from a hostile pack");
        assert!(
            got.iter().all(|&b| b == 0),
            "{what}: hostile bytes were written"
        );
    }
}

#[test]
fn a_wrong_whole_file_hash_is_an_error() {
    let a = rand_bytes(6, 3000);
    let mut e = entry(b"f.bin", Kind::File, 0o644, &a);
    e.blake3 = Some(blake3::hash(b"something else").to_hex().to_string());
    let (_, out) = run_case(&Repo::new(&[&a], vec![e]));
    assert_eq!(out.code, 3, "{}", out.text);
    assert!(out.text.contains("BLAKE3"), "{}", out.text);
}

fn case_insensitive(dir: &Path) -> bool {
    write(&dir.join("probe-CASE"), b"x");
    dir.join("probe-case").exists()
}

#[test]
fn a_case_colliding_pair_never_overwrites_one_with_the_other() {
    let (a, b) = (rand_bytes(7, 3000), rand_bytes(8, 3000));
    let r = Repo::new(
        &[&a, &b],
        vec![
            entry(b"Case.txt", Kind::File, 0o644, &a),
            entry(b"case.txt", Kind::File, 0o644, &b),
        ],
    );
    let (env, out) = run_case(&r);
    let insensitive = case_insensitive(env.dir.path());
    // macOS CI's APFS volume is case-insensitive, so the refusal branch runs there for certain
    if cfg!(target_os = "macos") && std::env::var_os("CI").is_some() {
        assert!(
            insensitive,
            "expected a case-insensitive volume on macOS CI"
        );
    }
    if insensitive {
        // refused: the second name is an error and the first keeps its own bytes
        assert_eq!(out.code, 3, "{}", out.text);
        assert!(out.text.contains("collision"), "{}", out.text);
        assert_eq!(std::fs::read(env.p("target/Case.txt")).expect("first"), a);
    } else {
        // a case-sensitive filesystem holds both names; nothing collides
        assert_eq!(out.code, 0, "{}", out.text);
        assert_eq!(std::fs::read(env.p("target/Case.txt")).expect("first"), a);
        assert_eq!(std::fs::read(env.p("target/case.txt")).expect("second"), b);
    }
}

#[test]
fn setid_bits_are_stripped_unless_kept() {
    use std::os::unix::fs::PermissionsExt;
    let a = rand_bytes(9, 3000);
    let entries = vec![
        entry(b"suid", Kind::File, 0o4755, &a),
        entry(b"sgid", Kind::File, 0o2755, &a),
    ];
    let r = Repo::new(&[&a], entries);
    let (env, out) = run_case(&r);
    assert_eq!(out.code, 0, "{}", out.text);
    let mode = |p: &str| {
        std::fs::metadata(env.p(p))
            .expect("stat")
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("target/suid"), 0o755);
    assert_eq!(mode("target/sgid"), 0o755);
    assert!(out.text.contains("stripped"), "{}", out.text);

    let kept = env.p("kept");
    let out = restore(&env, &kept, &["--keep-setid"]);
    assert_eq!(out.code, 0, "{}", out.text);
    assert_eq!(mode("kept/suid"), 0o4755);
    assert_eq!(
        mode("kept/sgid") & 0o2000,
        0o2000,
        "setgid dropped with --keep-setid"
    );
}

#[test]
fn a_non_empty_target_is_refused() {
    let a = rand_bytes(10, 3000);
    let env = Env::new();
    Repo::new(&[&a], vec![entry(b"f.bin", Kind::File, 0o644, &a)]).write(&env);
    write(&env.p("target/existing.txt"), b"already here\n");
    let out = restore(&env, &env.p("target"), &[]);
    assert_eq!(out.code, 3, "{}", out.text);
    assert_eq!(
        std::fs::read(env.p("target/existing.txt")).expect("kept"),
        b"already here\n"
    );
    assert!(!env.p("target/f.bin").exists());
}

/// Index objects of minimal JSON whose chunk entries the chunk map retains (they reference the
/// run's listed pack), `n` entries in all, spread over several objects of other runs.
/// Streamed straight into the store, so this process never holds the JSON.
fn fill_index(env: &Env, pack: &ObjRef, n: usize) {
    use std::io::Write;
    let per = n.div_ceil(4);
    for o in 0..4 {
        let p = env.p(&format!("store/v1/index/rfill{o}/000000.idx"));
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        let f = std::io::BufWriter::new(std::fs::File::create(&p).expect("create"));
        let mut z = zstd::stream::write::Encoder::new(f, 1).expect("encoder");
        write!(
            z,
            "{{\"v\":1,\"engine\":\"e\",\"run\":\"rfill{o}\",\"seq\":0,\"root\":\"r\",\"hash_kind\":\"sha1\",\"packs\":[{}],\"chunks\":[",
            serde_json::to_string(pack).expect("ref")
        )
        .expect("head");
        let (lo, hi) = (o * per, ((o + 1) * per).min(n));
        for i in lo..hi {
            let id = blake3::hash(&(i as u64).to_le_bytes()).to_hex();
            let sep = if i > lo { "," } else { "" };
            write!(z, "{sep}{{\"id\":\"{id}\",\"p\":0,\"len\":1}}").expect("chunk");
        }
        z.write_all(b"]}").expect("tail");
        z.finish().expect("finish").flush().expect("flush");
    }
}

fn share_case(extra: usize) -> (Env, Out) {
    let a = rand_bytes(11, 3000);
    let env = Env::new();
    let r = Repo::new(&[&a], vec![entry(b"f.bin", Kind::File, 0o644, &a)]);
    r.write(&env);
    let pk = pack_key(&r.pack);
    let i = xfer::repo::Budget::compute(640 << 20, 16 << 20, 8)
        .expect("budget")
        .i as usize;
    let fit = i / xfer::repo::CHUNK_ENTRY_COST as usize;
    fill_index(&env, &objref(&pk, &r.pack), fit + extra - 2000);
    let out = restore(&env, &env.p("target"), &[]);
    eprintln!("[{} MiB] share case +{extra}", out.rss >> 20);
    assert!(
        out.rss < RSS_MAX,
        "peak RSS {} MiB: {}",
        out.rss >> 20,
        out.text
    );
    (env, out)
}

#[test]
fn an_index_just_under_its_share_is_accepted() {
    let (env, out) = share_case(0);
    assert_eq!(out.code, 0, "{}", out.text);
    assert!(env.p("target/f.bin").exists());
}

#[test]
fn an_index_just_over_its_share_is_refused_before_the_target_exists() {
    let (env, out) = share_case(4000);
    assert_eq!(out.code, 3, "{}", out.text);
    assert!(out.text.contains("share"), "{}", out.text);
    assert!(!env.p("target").exists(), "the target was created");
}
