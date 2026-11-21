//! Artifact and cache rules, loaded from `rules/artifacts.toml` (data, not code).

use crate::error::{Error, Result};
use crate::model::Class;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

/// The rules shipped with the tool, embedded at build time.
const BUILTIN: &str = include_str!("../../../rules/artifacts.toml");

/// One regenerable-directory or cache rule.
#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    /// Directory name; a name containing `/` matches the trailing path components.
    pub name: String,
    /// Entries that must exist beside the directory or in an ancestor. Empty means none needed.
    #[serde(default)]
    pub markers: Vec<String>,
    /// `regenerable` or `cache`.
    pub class: Class,
    /// A build-output directory: output files of any size anywhere in it are listed (and make
    /// it ask), even inside a package directory. Every artifact asks for a listed output
    /// whatever this says; without it, an output is listed at any size unless it sits in an
    /// installed package's directory (`fixture_files`, `fixture_dirs`), where it is listed when
    /// it is at least the scan's output threshold. It also makes the directory ask when it sits
    /// in loose code with no git history.
    #[serde(default)]
    pub scan_outputs: bool,
    /// One line description.
    #[serde(default)]
    pub note: String,
    /// The directory can hold content that exists nowhere else (locally installed artifacts,
    /// settings, credentials), so it is never offered for removal: this is the reason shown.
    #[serde(default)]
    pub ask: Option<String>,
    /// Entries (`name` or `*.suffix`) that, found directly inside the matched directory, show it
    /// is not what the rule describes (a user-level home rather than a project cache); the
    /// directory is then never offered for removal.
    #[serde(default)]
    pub ask_if_contains: Vec<String>,
    /// Evidence that a file belongs to an installed package, for directories of third-party
    /// package contents. Package fixtures routinely use key, certificate and database names; a
    /// `fixture_prone` file name in `[[state_file]]` is exempt only when some directory between
    /// the file and this directory (exclusive) holds one of these entries (`name` or `*.suffix`).
    /// The same evidence lets an output file below the scan's output threshold go unlisted.
    #[serde(default)]
    pub fixture_files: Vec<String>,
    /// Like `fixture_files`, by directory name: the file sits in a package directory directly
    /// below a directory with one of these names (`site-packages`).
    #[serde(default)]
    pub fixture_dirs: Vec<String>,
}

/// A file-name pattern for unique state: a file that, found anywhere under a regenerable or
/// cache directory, means the directory is not only generated files. Matched on the name, never
/// on the contents.
#[derive(Debug, Clone, Deserialize)]
pub struct StateFile {
    /// `name`, `*suffix` or `prefix*`, compared without regard to case.
    pub pattern: String,
    /// What it is, in words for the reason shown to the user.
    pub kind: String,
    /// Names that match `pattern` but are not state (`.env.example`).
    #[serde(default)]
    pub except: Vec<String>,
    /// Package fixtures routinely use this name: it does not count when the file has package evidence (see `Rule::fixture_files`).
    #[serde(default)]
    pub fixture_prone: bool,
}

impl StateFile {
    /// Does `name` match this pattern (and none of its exceptions)?
    pub fn matches(&self, name: &str) -> bool {
        name_matches(&self.pattern, name)
            && !self.except.iter().any(|e| e.eq_ignore_ascii_case(name))
    }
}

/// `name`, `*suffix` or `prefix*`, ignoring ASCII case.
fn name_matches(spec: &str, name: &str) -> bool {
    let (s, n) = (spec.as_bytes(), name.as_bytes());
    if let Some(suffix) = spec.strip_prefix('*') {
        n.len() >= suffix.len()
            && n[n.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
    } else if let Some(prefix) = spec.strip_suffix('*') {
        n.len() >= prefix.len() && n[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    } else {
        s.eq_ignore_ascii_case(n)
    }
}

impl Rule {
    /// True when the trailing components of `path` equal the rule name.
    fn tail_matches(&self, path: &Path) -> bool {
        let mut comps = path.components().rev();
        for want in self.name.rsplit('/') {
            match comps.next() {
                Some(Component::Normal(c)) if c.to_str() == Some(want) => {}
                _ => return false,
            }
        }
        true
    }
}

/// A secret file name (`[[secret_name]]`): such an entry is always its own `keep` piece and is
/// never hashed, uploaded or pushed (docs/SCAN-FORMAT.md, "Secret names"). Matched on the name,
/// ignoring ASCII case, never on the contents.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretName {
    /// `name`, `*suffix`, `prefix*` or `*substring*`. A substring pattern applies outside
    /// regenerable and cache directories only.
    pub pattern: String,
    /// What it is, in words.
    pub kind: String,
    /// Names that match `pattern` but are not secrets (`.env.example`).
    #[serde(default)]
    pub except: Vec<String>,
}

impl SecretName {
    fn is_substring(&self) -> bool {
        self.pattern.len() > 2 && self.pattern.starts_with('*') && self.pattern.ends_with('*')
    }

    /// Does `name` match, inside (`in_artifact`) or outside an artifact directory?
    pub fn matches(&self, name: &str, in_artifact: bool) -> bool {
        if self.except.iter().any(|e| e.eq_ignore_ascii_case(name)) {
            return false;
        }
        if self.is_substring() {
            if in_artifact {
                return false;
            }
            let sub = self.pattern[1..self.pattern.len() - 1].to_ascii_lowercase();
            return name.to_ascii_lowercase().contains(&sub);
        }
        name_matches(&self.pattern, name)
    }
}

/// A secret word (`[[secret_word]]`): a file whose name holds one of `words` as a whole segment
/// says it holds a secret. Segments are the parts of the name's stem (the name without its last
/// extension) split at every character that is not a letter or digit and at each lower-to-upper
/// case change, compared ignoring ASCII case: `client_secret.json` and `clientSecret.json` hold
/// `secret`, `tokenizer.rs` and `tokens.ts` do not hold `token`. Applied outside regenerable and
/// cache directories only.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretWord {
    /// The segment forms that count (`secret`, `secrets`).
    pub words: Vec<String>,
    /// What it is, in words.
    pub kind: String,
    /// The word must be the stem's last segment (`api_token`, `token.json`) or follow one of
    /// `after` (`auth-token-cache.json`); `fix-token-refresh` does not hold it.
    #[serde(default)]
    pub last: bool,
    /// Segments after which the word counts anywhere (with `last`).
    #[serde(default)]
    pub after: Vec<String>,
    /// A name with one of these extensions matches outright (`deploy.token`).
    #[serde(default)]
    pub extensions: Vec<String>,
    /// In a file with a code or document extension (`[secrets] plain_extensions`), the word
    /// still counts when it is the whole stem (`secrets.py`, `credentials.ts`). Off for words
    /// that are ordinary file names in code (`token.rs` in a lexer).
    #[serde(default)]
    pub whole_stem_in_code: bool,
}

/// The lower-case segments of `stem` (see [`SecretWord`]).
fn name_segments(stem: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in stem.chars() {
        if !ch.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        cur.push(ch.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `name` split into stem and lower-case last extension; a leading dot is part of the stem
/// (`.token` has no extension).
fn stem_and_ext(name: &str) -> (&str, Option<String>) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => {
            (&name[..i], Some(name[i + 1..].to_ascii_lowercase()))
        }
        _ => (name, None),
    }
}

impl SecretWord {
    /// Does the file name `name` hold this word? `plain` are the code and document extensions.
    pub fn matches(&self, name: &str, plain: &HashSet<String>) -> bool {
        let (stem, ext) = stem_and_ext(name);
        if let Some(e) = &ext {
            if self.extensions.iter().any(|x| x.eq_ignore_ascii_case(e)) {
                return true;
            }
        }
        let segs = name_segments(stem);
        let is_word = |s: &str| self.words.iter().any(|w| w.eq_ignore_ascii_case(s));
        if ext.as_ref().map(|e| plain.contains(e)).unwrap_or(false) {
            return self.whole_stem_in_code && segs.len() == 1 && is_word(&segs[0]);
        }
        segs.iter().enumerate().any(|(i, s)| {
            is_word(s)
                && (!self.last
                    || i + 1 == segs.len()
                    || (i > 0
                        && self
                            .after
                            .iter()
                            .any(|a| a.eq_ignore_ascii_case(&segs[i - 1]))))
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretsCfg {
    /// Code and document extensions, where a secret word counts only as the whole stem.
    #[serde(default)]
    plain_extensions: Vec<String>,
}

/// A secret directory name (`[[secret_dir]]`): everything below it is one `keep` piece.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretDir {
    /// Exact directory name, ignoring ASCII case.
    pub name: String,
    /// What it is, in words.
    pub kind: String,
}

#[derive(Debug, Default, Deserialize)]
struct OutputsCfg {
    #[serde(default)]
    extensions: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LooseCfg {
    #[serde(default)]
    manifests: Vec<String>,
    #[serde(default)]
    source_extensions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawRules {
    #[serde(default)]
    rule: Vec<Rule>,
    #[serde(default)]
    outputs: OutputsCfg,
    #[serde(default)]
    loose_code: LooseCfg,
    #[serde(default)]
    state_file: Vec<StateFile>,
    #[serde(default)]
    secret_name: Vec<SecretName>,
    #[serde(default)]
    secret_dir: Vec<SecretDir>,
    #[serde(default)]
    secret_word: Vec<SecretWord>,
    #[serde(default)]
    secrets: SecretsCfg,
}

/// A compiled rule set with the lookup tables the scanner needs.
#[derive(Debug)]
pub struct RuleSet {
    rules: Vec<Rule>,
    by_last: HashMap<String, Vec<usize>>,
    output_ext: HashSet<String>,
    source_ext: HashSet<String>,
    manifest_specs: Vec<String>,
    marker_specs: Vec<String>,
    scan_dir_names: HashSet<String>,
    state_files: Vec<StateFile>,
    secret_names: Vec<SecretName>,
    secret_dirs: Vec<SecretDir>,
    secret_words: Vec<SecretWord>,
    /// Code and document extensions plus the loose-code source extensions.
    plain_ext: HashSet<String>,
}

/// Does a marker spec (`name` or `*.suffix`) match an entry name?
pub fn marker_matches(spec: &str, name: &str) -> bool {
    match spec.strip_prefix('*') {
        Some(suffix) => name.ends_with(suffix),
        None => spec == name,
    }
}

impl RuleSet {
    /// The rules embedded in the binary.
    pub fn builtin() -> RuleSet {
        // The embedded file is validated by a unit test, so a failure here is a build defect.
        RuleSet::from_toml(BUILTIN).expect("embedded rules/artifacts.toml must be valid")
    }

    /// Parse a rules file.
    pub fn from_toml(text: &str) -> Result<RuleSet> {
        let raw: RawRules = toml::from_str(text).map_err(|e| Error::Rules(e.to_string()))?;
        for r in &raw.rule {
            if !matches!(r.class, Class::Regenerable | Class::Cache) {
                return Err(Error::Rules(format!(
                    "rule {:?}: class must be regenerable or cache",
                    r.name
                )));
            }
            if r.name.is_empty() || r.name.starts_with('/') || r.name.ends_with('/') {
                return Err(Error::Rules(format!("rule name {:?} is not valid", r.name)));
            }
            if r.ask.as_deref().is_some_and(|a| a.trim().is_empty()) {
                return Err(Error::Rules(format!(
                    "rule {:?}: ask must give a reason",
                    r.name
                )));
            }
        }
        for sf in &raw.state_file {
            let stars = sf.pattern.matches('*').count();
            let edge = sf.pattern.starts_with('*') || sf.pattern.ends_with('*');
            if sf.pattern.is_empty()
                || sf.pattern.contains('/')
                || stars > 1
                || (stars == 1 && !edge)
                || sf.kind.trim().is_empty()
            {
                return Err(Error::Rules(format!(
                    "state_file {:?}: needs a name, `*suffix` or `prefix*` pattern and a kind",
                    sf.pattern
                )));
            }
        }
        for sn in &raw.secret_name {
            let p = &sn.pattern;
            let stars = p.matches('*').count();
            let ok_shape = match stars {
                0 => true,
                1 => p.starts_with('*') || p.ends_with('*'),
                2 => p.len() > 2 && p.starts_with('*') && p.ends_with('*'),
                _ => false,
            };
            if p.is_empty() || p.contains('/') || !ok_shape || sn.kind.trim().is_empty() {
                return Err(Error::Rules(format!(
                    "secret_name {p:?}: needs a name, `*suffix`, `prefix*` or `*substring*` \
                     pattern and a kind"
                )));
            }
        }
        for sw in &raw.secret_word {
            let bad = |w: &String| w.is_empty() || !w.chars().all(|c| c.is_ascii_alphanumeric());
            if sw.words.is_empty()
                || sw.words.iter().any(bad)
                || sw.after.iter().any(bad)
                || sw.extensions.iter().any(bad)
                || sw.kind.trim().is_empty()
            {
                return Err(Error::Rules(format!(
                    "secret_word {:?}: needs words, `after` and extensions of letters and digits, \
                     and a kind",
                    sw.words
                )));
            }
        }
        for sd in &raw.secret_dir {
            if sd.name.is_empty()
                || sd.name.contains('/')
                || sd.name.contains('*')
                || sd.kind.trim().is_empty()
            {
                return Err(Error::Rules(format!(
                    "secret_dir {:?}: needs a plain directory name and a kind",
                    sd.name
                )));
            }
        }
        let mut by_last: HashMap<String, Vec<usize>> = HashMap::new();
        let mut marker_specs: Vec<String> = Vec::new();
        let mut scan_dir_names = HashSet::new();
        for (i, r) in raw.rule.iter().enumerate() {
            let last = r.name.rsplit('/').next().unwrap_or(&r.name).to_string();
            if r.scan_outputs {
                scan_dir_names.insert(last.clone());
            }
            by_last.entry(last).or_default().push(i);
            // Entries named by `ask_if_contains` are recorded like markers, so the scanner sees
            // them among the matched directory's own entries.
            for m in r
                .markers
                .iter()
                .chain(&r.ask_if_contains)
                .chain(&r.fixture_files)
            {
                if !marker_specs.contains(m) {
                    marker_specs.push(m.clone());
                }
            }
        }
        Ok(RuleSet {
            rules: raw.rule,
            by_last,
            output_ext: raw
                .outputs
                .extensions
                .iter()
                .map(|e| e.to_ascii_lowercase())
                .collect(),
            source_ext: raw
                .loose_code
                .source_extensions
                .iter()
                .map(|e| e.to_ascii_lowercase())
                .collect(),
            manifest_specs: raw.loose_code.manifests,
            marker_specs,
            scan_dir_names,
            state_files: raw.state_file,
            secret_names: raw.secret_name,
            secret_dirs: raw.secret_dir,
            secret_words: raw.secret_word,
            plain_ext: raw
                .secrets
                .plain_extensions
                .iter()
                .chain(raw.loose_code.source_extensions.iter())
                .map(|e| e.to_ascii_lowercase())
                .collect(),
        })
    }

    /// All rules.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Rules whose name matches the end of `path` (markers are not checked here).
    pub fn candidates(&self, path: &Path) -> Vec<&Rule> {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return Vec::new(),
        };
        match self.by_last.get(name) {
            Some(idxs) => idxs
                .iter()
                .map(|&i| &self.rules[i])
                .filter(|r| r.tail_matches(path))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Is `name` an entry that some rule uses as a marker?
    pub fn is_marker(&self, name: &str) -> bool {
        self.marker_specs.iter().any(|s| marker_matches(s, name))
    }

    /// The state-file pattern (index into [`RuleSet::state_files`]) that `name` matches, if any.
    /// The first match in the order of the rules file wins.
    pub fn state_file(&self, name: &str) -> Option<usize> {
        self.state_files.iter().position(|sf| sf.matches(name))
    }

    /// All state-file patterns.
    pub fn state_files(&self) -> &[StateFile] {
        &self.state_files
    }

    /// The kind of secret file name `name` is, if any (`in_artifact`: the entry is inside a
    /// regenerable or cache directory, where substring patterns and secret words do not apply).
    pub fn secret_file(&self, name: &str, in_artifact: bool) -> Option<&str> {
        if let Some(s) = self
            .secret_names
            .iter()
            .find(|s| s.matches(name, in_artifact))
        {
            return Some(s.kind.as_str());
        }
        if in_artifact {
            return None;
        }
        self.secret_words
            .iter()
            .find(|w| w.matches(name, &self.plain_ext))
            .map(|w| w.kind.as_str())
    }

    /// All secret words.
    pub fn secret_words(&self) -> &[SecretWord] {
        &self.secret_words
    }

    /// The kind of secret directory name `name` is, if any.
    pub fn secret_dir(&self, name: &str) -> Option<&str> {
        self.secret_dirs
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .map(|s| s.kind.as_str())
    }

    /// All secret file-name patterns.
    pub fn secret_names(&self) -> &[SecretName] {
        &self.secret_names
    }

    /// Is `name` a project manifest (loose-code evidence)?
    pub fn is_manifest(&self, name: &str) -> bool {
        self.manifest_specs.iter().any(|s| marker_matches(s, name))
    }

    /// Is `name` a source file by extension?
    pub fn is_source_file(&self, name: &str) -> bool {
        extension_lower(name)
            .map(|e| self.source_ext.contains(&e))
            .unwrap_or(false)
    }

    /// Is `name` an output file by extension?
    pub fn is_output_file(&self, name: &str) -> bool {
        extension_lower(name)
            .map(|e| self.output_ext.contains(&e))
            .unwrap_or(false)
    }

    /// Does any component of `dir` carry the name of an output-scanning rule?
    /// A cheap superset test used before the real owner is known.
    pub fn in_output_scan_candidate(&self, dir: &Path) -> bool {
        dir.components().any(|c| match c {
            Component::Normal(n) => n
                .to_str()
                .map(|s| self.scan_dir_names.contains(s))
                .unwrap_or(false),
            _ => false,
        })
    }

    /// Is `dir`'s own name the last component of some artifact rule's name? A cheap superset
    /// test for "directly in an artifact's top directory", used before the owner is known.
    pub fn is_artifact_dir_name(&self, dir: &Path) -> bool {
        dir.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| self.by_last.contains_key(n))
    }

    /// Does any component of `dir` carry the last name component of some artifact rule? A cheap
    /// superset test for "somewhere inside an artifact", used before the owner is known.
    pub fn in_artifact_candidate(&self, dir: &Path) -> bool {
        dir.components().any(|c| match c {
            Component::Normal(n) => n.to_str().is_some_and(|s| self.by_last.contains_key(s)),
            _ => false,
        })
    }
}

fn extension_lower(name: &str) -> Option<String> {
    let idx = name.rfind('.')?;
    if idx == 0 || idx + 1 >= name.len() {
        return None;
    }
    Some(name[idx + 1..].to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_rules_parse_and_ambiguous_names_have_markers() {
        let rs = RuleSet::builtin();
        assert!(rs.rules().len() > 40);
        for name in ["dist", "build", "out", "target", "bin", "obj", "coverage"] {
            let rules: Vec<_> = rs.rules().iter().filter(|r| r.name == name).collect();
            assert!(!rules.is_empty(), "{name} has no rule");
            assert!(
                rules.iter().all(|r| !r.markers.is_empty()),
                "{name} must always require a marker"
            );
        }
    }

    #[test]
    fn multi_component_names_match_the_tail() {
        let rs = RuleSet::builtin();
        assert_eq!(rs.candidates(Path::new("/h/.cargo/registry")).len(), 1);
        assert!(rs.candidates(Path::new("/h/registry")).is_empty());
    }

    #[test]
    fn suffix_markers() {
        assert!(marker_matches("*.csproj", "App.csproj"));
        assert!(!marker_matches("*.csproj", "App.cs"));
        assert!(marker_matches("package.json", "package.json"));
    }

    #[test]
    fn stores_that_can_hold_unique_content_always_ask() {
        let rs = RuleSet::builtin();
        for name in [".m2/repository", ".terraform"] {
            let r = rs
                .rules()
                .iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("no rule {name}"));
            assert!(r.ask.is_some(), "{name} must ask");
        }
        let gradle = rs.rules().iter().find(|r| r.name == ".gradle").unwrap();
        assert!(gradle
            .ask_if_contains
            .iter()
            .any(|n| n == "gradle.properties"));
        assert!(rs.is_marker("gradle.properties"));
        let bad = "[[rule]]\nname = \"x\"\nclass = \"cache\"\nask = \" \"\n";
        assert!(RuleSet::from_toml(bad).is_err());
    }

    #[test]
    fn state_file_names_are_matched_by_name_only() {
        let rs = RuleSet::builtin();
        let kind = |n: &str| rs.state_file(n).map(|i| rs.state_files()[i].kind.clone());
        for hit in [
            "terraform.tfstate",
            "Terraform.TFSTATE.backup",
            "prod.tfvars",
            ".env",
            ".env.local",
            "app.sqlite",
            "app.sqlite3",
            "data.DB",
            "server.pem",
            "id.key",
            "a.keystore",
            "a.jks",
            "a.p12",
        ] {
            assert!(kind(hit).is_some(), "{hit} should be state");
        }
        for miss in [
            ".env.example",
            ".env.sample",
            ".env.template",
            "env",
            "main.tf",
            "db",
        ] {
            assert!(kind(miss).is_none(), "{miss} should not be state");
        }
        let fixture = |n: &str| rs.state_file(n).map(|i| rs.state_files()[i].fixture_prone);
        assert_eq!(fixture("cert.pem"), Some(true));
        assert_eq!(fixture("terraform.tfstate"), Some(false));
        assert_eq!(fixture(".env"), Some(false));
        let bad = "[[state_file]]\npattern = \"a*b\"\nkind = \"x\"\n";
        assert!(RuleSet::from_toml(bad).is_err());
    }

    #[test]
    fn fixture_evidence_is_defined_for_package_directories_only() {
        let rs = RuleSet::builtin();
        let rule = |n: &str| rs.rules().iter().find(|r| r.name == n).unwrap();
        assert!(rule("node_modules")
            .fixture_files
            .iter()
            .any(|f| f == "package.json"));
        assert!(rule(".venv")
            .fixture_dirs
            .iter()
            .any(|d| d == "site-packages"));
        assert!(rule("venv")
            .fixture_dirs
            .iter()
            .any(|d| d == "site-packages"));
        for name in ["build", "target", "dist", "obj", "Pods", ".npm"] {
            assert!(rs
                .rules()
                .iter()
                .filter(|r| r.name == name)
                .all(|r| r.fixture_files.is_empty() && r.fixture_dirs.is_empty()));
        }
    }

    #[test]
    fn rejects_non_artifact_class() {
        let bad = "[[rule]]\nname = \"x\"\nclass = \"repo\"\n";
        assert!(RuleSet::from_toml(bad).is_err());
    }

    #[test]
    fn extension_checks() {
        let rs = RuleSet::builtin();
        assert!(rs.is_output_file("clip.MP4"));
        assert!(!rs.is_output_file("mp4"));
        assert!(rs.is_source_file("main.rs"));
    }
}
