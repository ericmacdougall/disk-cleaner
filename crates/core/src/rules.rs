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
    /// Search this directory for output files before offering it for removal.
    #[serde(default)]
    pub scan_outputs: bool,
    /// One line description.
    #[serde(default)]
    pub note: String,
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
            for m in &r.markers {
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
