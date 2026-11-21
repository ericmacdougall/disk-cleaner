//! Transport through a private `rclone rcd`, so storage keys stay inside rclone.
//!
//! - rcd binds 127.0.0.1 on a free port and is trusted only after its own stderr reports serving
//!   on that port and `core/pid` equals the child we started.
//! - Authentication is an htpasswd `{SHA}` file (0600, in a 0700 temp dir) with random per-run
//!   credentials; they never appear in argv or the environment.
//! - Uploads go through `operations/uploadfile`. rcd runs with `--streaming-upload-cutoff` above
//!   the largest pack, so rclone buffers each pack and uploads it single-part with a known checksum
//!   and low-level retries. Backend cutoffs are pinned at 200 MiB so every pack is single-part.
//! - The engine never calls `config/*` and never reads rclone's config file.

use super::{RemoteObject, Transport};
use crate::error::{looks_throttled, Error, Result};
use crate::hash::HashKind;
use crate::util;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MIN_VERSION: (u32, u32) = (1, 60);
/// rc methods the engine may call. Anything else (notably `config/*`) is refused in code.
const ALLOWED: &[&str] = &[
    "rc/noop",
    "core/pid",
    "operations/fsinfo",
    "operations/stat",
    "operations/list",
    "operations/uploadfile",
];
const RESPONSE_MAX: u64 = 256 << 20;

#[cfg(unix)]
const WRAPPER: &str =
    "\"$0\" \"$@\" </dev/null & p=$!; echo \"$p\"; read _x; kill \"$p\" 2>/dev/null; wait \"$p\"";

pub struct RcloneRcd {
    child: Child,
    stdin: Option<ChildStdin>,
    rclone_pid: u32,
    base: String,
    auth: String,
    root: String,
    agent: ureq::Agent,
    kind: Option<HashKind>,
    tmpdir: PathBuf,
    version: String,
    log: Arc<Mutex<VecDeque<String>>>,
    args: Vec<String>,
}

/// Parse "rclone v1.60.1-DEV" to (1, 60).
pub fn parse_version(line: &str) -> Option<(u32, u32)> {
    let v = line
        .split_whitespace()
        .find(|w| w.starts_with('v'))?
        .trim_start_matches('v');
    let mut it = v.split(|c: char| !c.is_ascii_digit());
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn make_private_dir() -> Result<PathBuf> {
    let d = std::env::temp_dir().join(format!(
        "prunedisk-xfer-{}-{}",
        std::process::id(),
        util::random_hex(6)
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&d)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir(&d)?;
    Ok(d)
}

fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    use std::io::Write;
    o.open(path)?.write_all(data)?;
    Ok(())
}

impl RcloneRcd {
    /// The argv rcd is started with (for the hygiene test): no secret is ever part of it.
    pub fn args(&self) -> &[String] {
        &self.args
    }
    pub fn rclone_pid(&self) -> u32 {
        self.rclone_pid
    }
    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub fn start(bin: &str, root: &str, max_pack: u64) -> Result<RcloneRcd> {
        if root.is_empty() || root.contains("config/") {
            return Err(Error::Refused(format!("refusing repository {root:?}")));
        }
        let out = Command::new(bin)
            .arg("version")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Transport(format!("cannot run {bin}: {e}")))?;
        let first = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        let ver = parse_version(&first)
            .ok_or_else(|| Error::Transport(format!("cannot parse rclone version {first:?}")))?;
        if ver < MIN_VERSION {
            return Err(Error::Refused(format!(
                "rclone {}.{} is older than {}.{}",
                ver.0, ver.1, MIN_VERSION.0, MIN_VERSION.1
            )));
        }
        let tmpdir = make_private_dir()?;
        let user = format!("x{}", util::random_hex(8));
        let pass = util::random_hex(24);
        let sha = {
            use sha1::Digest;
            util::base64(&sha1::Sha1::digest(pass.as_bytes()))
        };
        let htpasswd = tmpdir.join("htpasswd");
        write_private(&htpasswd, format!("{user}:{{SHA}}{sha}\n").as_bytes())?;
        let auth = format!(
            "Basic {}",
            util::base64(format!("{user}:{pass}").as_bytes())
        );
        drop(pass);
        let cutoff_mib = max_pack.div_ceil(1 << 20) + 1;
        let mut last_err = String::new();
        for _ in 0..5 {
            let port = std::net::TcpListener::bind("127.0.0.1:0")?
                .local_addr()?
                .port();
            let args: Vec<String> = vec![
                "rcd".into(),
                "--rc-addr".into(),
                format!("127.0.0.1:{port}"),
                "--rc-htpasswd".into(),
                htpasswd.to_string_lossy().into_owned(),
                "--rc-serve".into(),
                "--streaming-upload-cutoff".into(),
                format!("{cutoff_mib}Mi"),
                "--b2-upload-cutoff".into(),
                "200Mi".into(),
                "--s3-upload-cutoff".into(),
                "200Mi".into(),
                "--log-level".into(),
                "NOTICE".into(),
            ];
            match Self::spawn(bin, &args, port, &tmpdir, &auth, root, first.clone()) {
                Ok(r) => return Ok(r),
                Err(e) => last_err = e.to_string(),
            }
        }
        let _ = std::fs::remove_dir_all(&tmpdir);
        Err(Error::Transport(format!(
            "rclone rcd did not start: {last_err}"
        )))
    }

    fn spawn(
        bin: &str,
        args: &[String],
        port: u16,
        tmpdir: &std::path::Path,
        auth: &str,
        root: &str,
        version: String,
    ) -> Result<RcloneRcd> {
        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(WRAPPER).arg(bin).args(args);
            c
        };
        #[cfg(not(unix))]
        let mut cmd = {
            let mut c = Command::new(bin);
            c.args(args);
            c
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Transport(format!("spawn rclone: {e}")))?;
        let stdin = child.stdin.take();
        #[cfg(unix)]
        let rclone_pid: u32 = {
            let mut line = String::new();
            let so = child
                .stdout
                .take()
                .ok_or_else(|| Error::Bug("no stdout".into()))?;
            BufReader::new(so).read_line(&mut line)?;
            line.trim()
                .parse()
                .map_err(|_| Error::Transport("rcd wrapper gave no pid".into()))?
        };
        #[cfg(not(unix))]
        let rclone_pid = child.id();
        let log = Arc::new(Mutex::new(VecDeque::new()));
        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        let se = child
            .stderr
            .take()
            .ok_or_else(|| Error::Bug("no stderr".into()))?;
        {
            let log = log.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(se).lines().map_while(|l| l.ok()) {
                    let mut l = log.lock().expect("log");
                    if l.len() >= 64 {
                        l.pop_front();
                    }
                    l.push_back(line.clone());
                    drop(l);
                    let _ = tx.send(line);
                }
            });
        }
        let base = format!("http://127.0.0.1:{port}/");
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(900))
            .timeout_write(Duration::from_secs(900))
            .build();
        let mut me = RcloneRcd {
            child,
            stdin,
            rclone_pid,
            base,
            auth: auth.to_string(),
            root: root.to_string(),
            agent,
            kind: None,
            tmpdir: tmpdir.to_path_buf(),
            version,
            log,
            args: args.to_vec(),
        };
        // Ready only when rclone itself says it serves on our port.
        let want = format!("127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(l) if l.contains("Serving remote control on") && l.contains(&want) => break,
                Ok(_) => {}
                Err(_) => {
                    if let Ok(Some(st)) = me.child.try_wait() {
                        return Err(Error::Transport(format!(
                            "rcd exited ({st}): {}",
                            me.recent_log()
                        )));
                    }
                    if Instant::now() > deadline {
                        return Err(Error::Transport(format!(
                            "rcd not ready in 30 s: {}",
                            me.recent_log()
                        )));
                    }
                }
            }
        }
        let pid = me.rc("core/pid", json!({}))?;
        if pid.get("pid").and_then(Value::as_u64) != Some(me.rclone_pid as u64) {
            return Err(Error::Refused(
                "the process answering on the rc port is not our rclone".into(),
            ));
        }
        let info = me.rc("operations/fsinfo", json!({ "fs": me.root }))?;
        let hashes: Vec<String> = info
            .get("Hashes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_ascii_lowercase))
                    .collect()
            })
            .unwrap_or_default();
        me.kind = if hashes.iter().any(|h| h == "sha1") {
            Some(HashKind::Sha1)
        } else if hashes.iter().any(|h| h == "md5") {
            Some(HashKind::Md5)
        } else {
            None
        };
        Ok(me)
    }

    fn recent_log(&self) -> String {
        self.log
            .lock()
            .map(|l| l.iter().cloned().collect::<Vec<_>>().join(" | "))
            .unwrap_or_default()
    }

    fn map_err(&self, what: &str, e: ureq::Error) -> Error {
        let text = match e {
            ureq::Error::Status(code, resp) => {
                let mut body = String::new();
                let _ = resp.into_reader().take(64 << 10).read_to_string(&mut body);
                format!("{what}: HTTP {code}: {body}")
            }
            ureq::Error::Transport(t) => format!("{what}: {t}"),
        };
        if looks_throttled(&text) {
            Error::Throttled(text)
        } else {
            Error::Transport(text)
        }
    }

    fn read_json(&self, what: &str, resp: ureq::Response) -> Result<Value> {
        let mut buf = Vec::new();
        resp.into_reader()
            .take(RESPONSE_MAX + 1)
            .read_to_end(&mut buf)?;
        if buf.len() as u64 > RESPONSE_MAX {
            return Err(Error::Transport(format!("{what}: response too large")));
        }
        serde_json::from_slice(&buf).map_err(|e| Error::Transport(format!("{what}: bad JSON: {e}")))
    }

    /// Call an allowed rc method with a JSON body.
    pub fn rc(&self, method: &str, body: Value) -> Result<Value> {
        if !ALLOWED.contains(&method) {
            return Err(Error::Refused(format!("rc method {method} is not allowed")));
        }
        let r = self
            .agent
            .post(&format!("{}{}", self.base, method))
            .set("Authorization", &self.auth)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(|e| self.map_err(method, e))?;
        self.read_json(method, r)
    }

    fn obj(&self, key: &str, v: &Value, kind: Option<HashKind>) -> Option<RemoteObject> {
        let size = v.get("Size").and_then(Value::as_i64)?;
        let hash = kind.and_then(|k| {
            v.get("Hashes")
                .and_then(|h| h.get(k.name()))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        Some(RemoteObject {
            key: key.to_string(),
            size: size.max(0) as u64,
            hash,
        })
    }
}

impl Transport for RcloneRcd {
    fn hash_kind(&self) -> Option<HashKind> {
        self.kind
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let (dir, name) = key.rsplit_once('/').unwrap_or(("", key));
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
        {
            return Err(Error::Refused(format!(
                "object name {name:?} is not a plain name"
            )));
        }
        let boundary = format!("xfer{}", util::random_hex(12));
        let head = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file0\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        );
        let tail = format!("\r\n--{boundary}--\r\n");
        let len = head.len() + data.len() + tail.len();
        let body = std::io::Cursor::new(head.into_bytes())
            .chain(data)
            .chain(std::io::Cursor::new(tail.into_bytes()));
        let url = format!(
            "{}operations/uploadfile?fs={}&remote={}",
            self.base,
            pct(&self.root),
            pct(dir)
        );
        self.agent
            .post(&url)
            .set("Authorization", &self.auth)
            .set(
                "Content-Type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .set("Content-Length", &len.to_string())
            .send(body)
            .map_err(|e| self.map_err("operations/uploadfile", e))?;
        Ok(())
    }

    fn stat(&self, key: &str) -> Result<Option<RemoteObject>> {
        let v = self.rc(
            "operations/stat",
            json!({"fs": self.root, "remote": key, "opt": {"showHash": true, "noMimeType": true}}),
        )?;
        match v.get("item") {
            None | Some(Value::Null) => Ok(None),
            Some(item) => Ok(self.obj(key, item, self.kind)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<RemoteObject>> {
        let r = self.rc(
            "operations/list",
            json!({"fs": self.root, "remote": prefix, "opt": {
                "recurse": true, "showHash": true, "filesOnly": true, "noMimeType": true, "noModTime": true}}),
        );
        let v = match r {
            Ok(v) => v,
            Err(Error::Transport(t))
                if t.contains("directory not found") || t.contains("not found") =>
            {
                return Ok(Vec::new())
            }
            Err(e) => return Err(e),
        };
        let items = v
            .get("list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let p = it.get("Path").and_then(Value::as_str).unwrap_or("");
            let key = if prefix.is_empty() {
                p.to_string()
            } else {
                format!("{}/{}", prefix.trim_end_matches('/'), p)
            };
            if let Some(o) = self.obj(&key, &it, self.kind) {
                out.push(o);
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn get(&self, key: &str, max: u64) -> Result<Vec<u8>> {
        let url = format!(
            "{}{}/{}",
            self.base,
            pct(&format!("[{}]", self.root)),
            pct(key)
        );
        let r = self
            .agent
            .get(&url)
            .set("Authorization", &self.auth)
            .call()
            .map_err(|e| self.map_err("get", e))?;
        let mut buf = Vec::new();
        r.into_reader().take(max + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > max {
            return Err(Error::corrupt(format!(
                "{key} is larger than the {max} byte limit"
            )));
        }
        Ok(buf)
    }

    fn describe(&self) -> String {
        format!("{} rcd -> {}", self.version, self.root)
    }
}

impl Drop for RcloneRcd {
    fn drop(&mut self) {
        // Closing the wrapper's stdin makes it kill rclone; then reap the wrapper.
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.tmpdir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_and_encoding() {
        assert_eq!(parse_version("rclone v1.60.1-DEV"), Some((1, 60)));
        assert_eq!(parse_version("rclone v1.75.2"), Some((1, 75)));
        assert!(parse_version("nonsense").is_none());
        assert_eq!(pct("[b2:bkt/x y]"), "%5Bb2%3Abkt/x%20y%5D");
    }
}
