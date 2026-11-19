//! Reading a scan file made elsewhere (desktop "Open scan" and `tui --scan`).
//!
//! The file is untrusted. It is opened without blocking on special files, must be a regular
//! file, is read with a size limit enforced while reading, and its parse's peak memory is
//! estimated in one cheap pass before core's `scan_from_json` sees it.

use crate::sanitize::display_name;
use disk_cleaner_core::ScanResult;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Largest scan file read: 1 GiB.
pub const MAX_IMPORT_BYTES: u64 = 1 << 30;

/// Most memory the parse may be estimated to need: 4 GiB.
pub const IMPORT_BUDGET: u64 = 4 << 30;

/// Limits for one import (tests lower them).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: u64,
    pub budget: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bytes: MAX_IMPORT_BYTES,
            budget: IMPORT_BUDGET,
        }
    }
}

/// Why an import failed. Every message is display-safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// Refused before or while reading (special file, too big, too costly).
    Refused(String),
    /// The file could not be opened or read.
    Io(String),
    /// Core refused the content (not JSON, wrong schema, a verification claim, a cycle).
    Scan(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Refused(m) | ImportError::Io(m) | ImportError::Scan(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ImportError {}

/// Windows device and pipe namespaces (`\\.\`, `\\?\GLOBALROOT`, `\\?\pipe\`...) are refused
/// before anything is opened. `\\?\C:\...` and `\\?\UNC\...` are ordinary paths.
pub fn is_device_namespace(path: &str) -> bool {
    let p = path.replace('/', "\\");
    if p.starts_with("\\\\.\\") {
        return true;
    }
    if let Some(rest) = p.strip_prefix("\\\\?\\") {
        let bytes = rest.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        let unc = rest
            .get(..4)
            .is_some_and(|s| s.eq_ignore_ascii_case("UNC\\"));
        return !(drive || unc);
    }
    false
}

/// Open a scan file for reading without blocking on a FIFO or device, and refuse anything but
/// a regular file. The only open in this crate besides the settings and export functions.
#[allow(clippy::disallowed_methods)] // read-only open, listed in scripts/readonly-allow.txt
pub fn open_scan_file(path: &Path) -> Result<File, ImportError> {
    let shown = display_name(&path.to_string_lossy());
    if is_device_namespace(&path.to_string_lossy()) {
        return Err(ImportError::Refused(format!(
            "{shown} is a device or pipe name, not a scan file"
        )));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let file = opts
        .open(path)
        .map_err(|e| ImportError::Io(format!("cannot open {shown}: {e}")))?;
    let md = file
        .metadata()
        .map_err(|e| ImportError::Io(format!("cannot read {shown}: {e}")))?;
    if !md.is_file() {
        return Err(ImportError::Refused(format!(
            "{shown} is not a regular file (a folder, pipe or device cannot be a scan file)"
        )));
    }
    Ok(file)
}

/// Read at most `max` bytes; a longer file, or one that grows past `max` while it is read, is
/// refused.
pub fn read_limited(mut file: File, max: u64, shown: &str) -> Result<Vec<u8>, ImportError> {
    if let Ok(md) = file.metadata() {
        if md.len() > max {
            return Err(too_big(shown, max));
        }
    }
    read_capped(&mut file, max, shown)
}

fn too_big(shown: &str, max: u64) -> ImportError {
    ImportError::Refused(format!(
        "{shown} is larger than the {} import limit",
        human(max)
    ))
}

/// Read at most `max` bytes from `r`, refusing when there is more: the limit holds while
/// reading, whatever the file's length said before.
pub fn read_capped(r: &mut dyn Read, max: u64, shown: &str) -> Result<Vec<u8>, ImportError> {
    let mut buf = Vec::new();
    r.take(max.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| ImportError::Io(format!("cannot read {shown}: {e}")))?;
    if buf.len() as u64 > max {
        return Err(too_big(shown, max));
    }
    Ok(buf)
}

/// What one pass over the text found, and the parse's estimated peak memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Estimate {
    /// Every value: scalars, arrays and objects.
    pub values: u64,
    pub objects: u64,
    pub keys: u64,
    pub array_elements: u64,
    pub peak_bytes: u64,
}

/// Bytes charged per array element (a `Value` in a `Vec` that may have doubled).
pub const ELEMENT_COST: u64 = 64;
/// Bytes per B-tree leaf of a parsed object (11 keys and values of 24 and 32 bytes).
pub const LEAF_COST: u64 = 640;
/// Bytes per B-tree internal node.
pub const INTERNAL_COST: u64 = 736;
/// Fixed overhead.
const BASE_COST: u64 = 4096;

/// Memory for one parsed object with `keys` members, assuming leaves as empty as a B-tree
/// allows (five keys).
pub fn object_cost(keys: u64) -> u64 {
    if keys <= 11 {
        return LEAF_COST;
    }
    let leaves = keys.div_ceil(5);
    let internals = leaves.div_ceil(5);
    leaves * LEAF_COST + internals * INTERNAL_COST
}

/// Count the values of a JSON text in one pass (no allocation per value) and estimate the
/// peak memory of parsing it into `serde_json::Value` and then a scan: twice the text (the text
/// and its strings) plus the arrays' and objects' own memory. Malformed text gives some
/// estimate; core reports the error.
pub fn estimate(text: &[u8]) -> Estimate {
    // Per open container: Some(keys) for an object, None for an array.
    let mut stack: Vec<Option<u64>> = Vec::new();
    let mut est = Estimate::default();
    let mut cost: u64 = BASE_COST.saturating_add((text.len() as u64).saturating_mul(2));
    let mut in_string = false;
    let mut escape = false;
    let mut in_scalar = false;
    for &b in text {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        let starts_value = match b {
            b'"' | b'{' | b'[' => {
                in_scalar = false;
                true
            }
            b'0'..=b'9' | b'-' | b't' | b'f' | b'n' => {
                let start = !in_scalar;
                in_scalar = true;
                start
            }
            b'.' | b'e' | b'E' | b'+' | b'r' | b'u' | b'a' | b'l' | b's' => false,
            _ => {
                in_scalar = false;
                false
            }
        };
        if starts_value {
            est.values += 1;
            // A string right after `{` or `,` in an object is a key; it is counted at `:`.
            if let Some(None) | None = stack.last() {
                est.array_elements += 1;
                cost = cost.saturating_add(ELEMENT_COST);
            }
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                est.objects += 1;
                stack.push(Some(0));
            }
            b'[' => stack.push(None),
            b':' => {
                if let Some(Some(k)) = stack.last_mut() {
                    *k += 1;
                    est.keys += 1;
                }
            }
            b'}' | b']' => {
                if let Some(Some(keys)) = stack.pop() {
                    cost = cost.saturating_add(object_cost(keys));
                }
            }
            _ => {}
        }
    }
    // Objects left open by a truncated file still cost something.
    for keys in stack.into_iter().flatten() {
        cost = cost.saturating_add(object_cost(keys));
    }
    est.peak_bytes = cost;
    est
}

fn human(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

/// A scan read from a file.
#[derive(Debug)]
pub struct Imported {
    pub scan: ScanResult,
    pub file_bytes: u64,
    pub estimate: Estimate,
}

/// Check bytes already read and parse them with core.
pub fn parse_bytes(bytes: Vec<u8>, limits: Limits, shown: &str) -> Result<Imported, ImportError> {
    if bytes.len() as u64 > limits.max_bytes {
        return Err(too_big(shown, limits.max_bytes));
    }
    let est = estimate(&bytes);
    if est.peak_bytes > limits.budget {
        return Err(ImportError::Refused(format!(
            "{shown} would need about {} of memory to read ({} values), more than the {} \
             import limit",
            human(est.peak_bytes),
            est.values,
            human(limits.budget)
        )));
    }
    let file_bytes = bytes.len() as u64;
    let text = String::from_utf8(bytes)
        .map_err(|_| ImportError::Scan(format!("{shown} is not UTF-8 text, so not a scan file")))?;
    let scan = disk_cleaner_core::scan_from_json(&text)
        .map_err(|e| ImportError::Scan(display_name(&format!("{shown}: {e}"))))?;
    Ok(Imported {
        scan,
        file_bytes,
        estimate: est,
    })
}

/// The whole import protocol: open, read with the limit, estimate, parse.
pub fn import(path: &Path, limits: Limits) -> Result<Imported, ImportError> {
    let shown = display_name(&path.to_string_lossy());
    let file = open_scan_file(path)?;
    let bytes = read_limited(file, limits.max_bytes, &shown)?;
    parse_bytes(bytes, limits, &shown)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests write fixtures
mod tests {
    use super::*;
    use crate::synth::{synthetic_scan, SEED};

    #[test]
    fn device_namespaces_are_recognised() {
        assert!(is_device_namespace(r"\\.\PhysicalDrive0"));
        assert!(is_device_namespace(r"\\.\pipe\x"));
        assert!(is_device_namespace(r"\\?\GLOBALROOT\Device\Null"));
        assert!(is_device_namespace(r"\\?\pipe\x"));
        assert!(is_device_namespace("//./CON"));
        assert!(!is_device_namespace(r"\\?\C:\scans\a.json"));
        assert!(!is_device_namespace(r"\\?\UNC\server\share\a.json"));
        assert!(!is_device_namespace(r"C:\scans\a.json"));
        assert!(!is_device_namespace("/home/x/a.json"));
    }

    #[test]
    fn estimate_counts_values_keys_and_elements() {
        let e = estimate(br#"{"a": [1, -2.5e3, true, null, "x\"y"], "b": {"c": false}}"#);
        assert_eq!(e.objects, 2);
        assert_eq!(e.keys, 3);
        // root object, array, 5 scalars, inner object, false
        // two objects, one array, five array scalars, three keys, one `false`
        assert_eq!(e.values, 2 + 1 + 5 + 3 + 1);
        assert_eq!(e.array_elements, 1 + 5);
    }

    #[test]
    fn a_file_of_tiny_values_is_refused_before_parsing() {
        // About 2 bytes per value: [0,0,0,...]
        let n = 200_000;
        let mut text = String::from("[");
        for i in 0..n {
            if i > 0 {
                text.push(',');
            }
            text.push('0');
        }
        text.push(']');
        let limits = Limits {
            max_bytes: MAX_IMPORT_BYTES,
            budget: 8 << 20,
        };
        match parse_bytes(text.into_bytes(), limits, "tiny.json") {
            Err(ImportError::Refused(m)) => assert!(m.contains("memory"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bad_content_gets_cores_message() {
        let l = Limits::default();
        assert!(matches!(
            parse_bytes(b"not json".to_vec(), l, "a"),
            Err(ImportError::Scan(_))
        ));
        let mut scan = synthetic_scan(3, SEED);
        scan.schema_version = 14;
        let text = serde_json::to_vec(&scan).unwrap();
        match parse_bytes(text, l, "old.json") {
            Err(ImportError::Scan(m)) => assert!(m.contains("14"), "{m}"),
            other => panic!("{other:?}"),
        }
        let mut v = serde_json::to_value(synthetic_scan(3, SEED)).unwrap();
        v["verified"] = serde_json::Value::Bool(true);
        assert!(matches!(
            parse_bytes(serde_json::to_vec(&v).unwrap(), l, "v.json"),
            Err(ImportError::Scan(_))
        ));
        let mut cyc = synthetic_scan(3, SEED);
        cyc.nodes[0].parent = Some(1);
        cyc.nodes[1].parent = Some(0);
        assert!(matches!(
            parse_bytes(serde_json::to_vec(&cyc).unwrap(), l, "c.json"),
            Err(ImportError::Scan(_))
        ));
        assert!(matches!(
            parse_bytes(vec![0xff, 0xfe], l, "b.json"),
            Err(ImportError::Scan(_))
        ));
    }

    #[test]
    fn a_good_file_round_trips_and_the_limit_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("scan.json");
        std::fs::write(&p, serde_json::to_vec(&synthetic_scan(50, SEED)).unwrap()).unwrap();
        let got = import(&p, Limits::default()).unwrap();
        assert_eq!(got.scan.nodes.len(), 50);
        let small = Limits {
            max_bytes: 100,
            budget: IMPORT_BUDGET,
        };
        match import(&p, small) {
            Err(ImportError::Refused(m)) => assert!(m.contains("larger than"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Opening a directory read-only works on Unix and fails on Windows: either way it is
        // not imported.
        assert!(import(dir.path(), Limits::default()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_with_no_writer_returns_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fifo");
        let status = std::process::Command::new("mkfifo").arg(&p).status().unwrap();
        assert!(status.success());
        let start = std::time::Instant::now();
        match import(&p, Limits::default()) {
            Err(ImportError::Refused(m)) => assert!(m.contains("not a regular file"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn the_limit_holds_while_reading() {
        // A reader gives no length up front, like a file that grows after its length was
        // checked: the cap must still stop it.
        let mut grown = std::io::Cursor::new(vec![b' '; 150]);
        match read_capped(&mut grown, 120, "grow.json") {
            Err(ImportError::Refused(m)) => assert!(m.contains("larger than"), "{m}"),
            other => panic!("{other:?}"),
        }
        let mut exact = std::io::Cursor::new(vec![b' '; 120]);
        assert_eq!(read_capped(&mut exact, 120, "ok.json").unwrap().len(), 120);
    }

    #[test]
    fn object_cost_grows_with_keys() {
        assert_eq!(object_cost(0), LEAF_COST);
        assert_eq!(object_cost(11), LEAF_COST);
        assert!(object_cost(12) > LEAF_COST);
        assert!(object_cost(1000) >= 200 * LEAF_COST);
    }
}
