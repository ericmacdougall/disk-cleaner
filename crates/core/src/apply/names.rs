//! Secret names, recomputed by apply for every entry it would upload or push (C2, NEG-1). The
//! plan's `secret` flag is never trusted to be complete. The list is the one documented in
//! docs/SCAN-FORMAT.md ("Secret names"), applied everywhere (including artifact directories,
//! which only over-matches: the safe side). It is replaced by stream A's core function when that
//! lands, with a test that the two agree.

/// Secret directory names.
const SECRET_DIRS: [&str; 5] = [".ssh", ".gnupg", ".aws", ".password-store", "secrets"];

const SUFFIXES: [&str; 8] = [
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".jks",
    ".keystore",
    ".kdbx",
    ".tfstate",
];

const EXACT: [&str; 9] = [
    ".netrc",
    "_netrc",
    ".pgpass",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    "credentials",
    "credentials.json",
    ".dev.vars",
];

const PREFIXES: [&str; 5] = [
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "terraform.tfstate.",
];

const CONTAINS: [&str; 5] = ["secret", "credential", "password", "passwd", "token"];

/// `.env.example` and friends are not secrets.
const ENV_EXAMPLES: [&str; 6] = [
    "example", "sample", "template", "dist", "default", "defaults",
];

/// Is `name` (one path component) a secret file name?
pub fn is_secret_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n == ".env" {
        return true;
    }
    if let Some(rest) = n.strip_prefix(".env.") {
        return !ENV_EXAMPLES
            .iter()
            .any(|e| rest == *e || rest.ends_with(&format!(".{e}")));
    }
    if n.ends_with(".tfvars") || n.ends_with(".tfvars.json") {
        return true;
    }
    SUFFIXES.iter().any(|s| n.ends_with(s) && n.len() > s.len())
        || EXACT.contains(&n.as_str())
        || PREFIXES.iter().any(|p| n.starts_with(p))
        || CONTAINS.iter().any(|c| n.contains(c))
}

/// The same rule without the substring patterns, for names inside artifact directories (the
/// scan applies substring rules only outside artifact nodes).
pub fn is_secret_file_strict(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n == ".env" || (n.starts_with(".env.") && is_secret_file(&n)) {
        return true;
    }
    n.ends_with(".tfvars")
        || n.ends_with(".tfvars.json")
        || SUFFIXES.iter().any(|s| n.ends_with(s) && n.len() > s.len())
        || EXACT.contains(&n.as_str())
        || PREFIXES.iter().any(|p| n.starts_with(p))
}

/// Like [`rel_is_secret`] with [`is_secret_file_strict`] for the file name.
pub fn rel_is_secret_strict(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let (last, dirs) = parts.split_last().expect("split always yields one part");
    dirs.iter().any(|d| is_secret_dir(d)) || is_secret_file_strict(last) || is_secret_dir(last)
}

/// Is `name` a secret directory name?
pub fn is_secret_dir(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SECRET_DIRS.contains(&n.as_str())
}

/// Does any component of the `/`-separated relative path name a secret (a secret file as the
/// last component, a secret directory anywhere)?
pub fn rel_is_secret(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let (last, dirs) = parts.split_last().expect("split always yields one part");
    dirs.iter().any(|d| is_secret_dir(d)) || is_secret_file(last) || is_secret_dir(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_names_match() {
        for s in [
            ".env",
            ".env.local",
            ".ENV.production",
            "id_ed25519",
            "id_rsa.pub",
            "x.pem",
            "credentials.json",
            ".npmrc",
            "my_token.txt",
            "prod.tfvars",
            "terraform.tfstate.backup",
        ] {
            assert!(is_secret_file(s), "{s} should be secret");
        }
        for s in [
            ".env.example",
            ".env.sample",
            "README.md",
            "main.rs",
            "key",
            "pem",
        ] {
            assert!(!is_secret_file(s), "{s} should not be secret");
        }
        assert!(rel_is_secret("a/.ssh/config"));
        assert!(rel_is_secret("a/b/.env"));
        assert!(!rel_is_secret("a/b/c.txt"));
    }
}
