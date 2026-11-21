//! Number and date formatting shared by the terminal view (the desktop app formats in its
//! frontend with the same rules).

/// Bytes in binary units: `0 B`, `1023 B`, `1.0 KiB`, `12.3 GiB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// A count with thousands separators: `1,234,567`.
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Percent with one decimal: `42.0%`.
pub fn percent(p: f64) -> String {
    format!("{:.1}%", p.clamp(0.0, 100.0))
}

/// Age of a unix time relative to `now`: `today`, `3 days`, `5 months`, `2 years`; `-` when
/// unknown or in the future.
pub fn age(mtime: Option<i64>, now: i64) -> String {
    let Some(t) = mtime else {
        return "-".to_string();
    };
    if t > now {
        return "-".to_string();
    }
    let days = (now - t) / 86_400;
    match days {
        0 => "today".to_string(),
        1 => "1 day".to_string(),
        2..=60 => format!("{days} days"),
        61..=730 => format!("{} months", days / 30),
        _ => format!("{} years", days / 365),
    }
}

/// An age for a sentence: "today", "3 days ago", or "at an unknown time" when unknown or in
/// the future.
pub fn ago(mtime: Option<i64>, now: i64) -> String {
    match age(mtime, now) {
        s if s == "today" => s,
        s if s == "-" => "at an unknown time".to_string(),
        s => format!("{s} ago"),
    }
}

/// The current time in unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(bytes(u64::MAX), "16.0 EiB");
        assert_eq!(count(0), "0");
        assert_eq!(count(1234), "1,234");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(percent(150.0), "100.0%");
        assert_eq!(age(None, 100), "-");
        assert_eq!(age(Some(0), 3 * 86_400), "3 days");
        assert_eq!(age(Some(200), 100), "-");
        assert_eq!(ago(Some(100), 200), "today");
        assert_eq!(ago(Some(0), 86_400), "1 day ago");
        assert_eq!(ago(Some(0), 3 * 86_400), "3 days ago");
        assert_eq!(ago(None, 100), "at an unknown time");
        assert_eq!(ago(Some(200), 100), "at an unknown time");
    }
}
