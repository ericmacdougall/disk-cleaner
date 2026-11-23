//! Step 3 of the per-item sequence: is the quarantined copy still what the plan said it was?
//!
//! The copy is judged by the scan's own classifier, not by a second implementation: the rule
//! set is copied with the item's rule renamed to the quarantine directory's name (and its marker
//! requirement moved to a check of the parent, where the marker still stands), and `scan` runs
//! on the quarantine directory as its root. The node it makes of that directory must still be
//! the same rule's regenerable or cache directory with nothing held back: no output file, state
//! file, ask rule, nested repository, unreadable entry, skipped mount or agent file. A contract
//! test (`classifier_agrees_with_the_scan`) runs the same fixtures through the plan-time scan and
//! this check and requires the same verdict.

use super::fsops::Dir;
use crate::model::{Class, Node, ScanOptionsRecord};
use crate::protect::ScanContext;
use crate::rules::{marker_matches, RuleSet};
use crate::scan::{scan, ScanOptions};
use std::path::Path;
use std::sync::Arc;

/// What the check needs from the run.
pub struct Classifier<'a> {
    pub rules: &'a RuleSet,
    pub options: &'a ScanOptionsRecord,
    pub context: &'a ScanContext,
}

impl Classifier<'_> {
    /// `Ok(())` when the directory `qname` (in `parent`, at `qpath`) is still an artifact of the
    /// rule `rule_id` and nothing in it holds it back. `Err` says why not, in words.
    pub fn verify_copy(
        &self,
        rule_id: &str,
        parent: &Dir,
        qname: &str,
        qpath: &Path,
    ) -> Result<(), String> {
        let rule = self
            .rules
            .by_id(rule_id)
            .ok_or_else(|| format!("rule {rule_id:?} is unknown to this build"))?;
        if !rule.markers.is_empty() {
            let names = parent
                .names()
                .map_err(|e| format!("cannot list the parent for the rule marker: {e}"))?;
            let found = names.iter().any(|n| {
                n.to_str()
                    .is_some_and(|n| rule.markers.iter().any(|m| marker_matches(m, n)))
            });
            if !found {
                return Err(format!(
                    "the marker of rule {rule_id} ({}) is gone from the parent",
                    rule.markers.join(", ")
                ));
            }
        }
        if rule.ask.is_some() {
            return Err(format!("rule {rule_id} asks for a person"));
        }
        let aliased = self
            .rules
            .with_alias(rule_id, qname)
            .ok_or_else(|| format!("rule {rule_id:?} cannot be aliased"))?;
        let mut opts = ScanOptions::new(vec![qpath.to_path_buf()]);
        opts.rules = Arc::new(aliased);
        opts.one_file_system = self.options.one_file_system;
        opts.top_files = self.options.top_files;
        opts.min_output_bytes = self.options.min_output_bytes;
        // Git facts judge repositories, which an artifact directory is not; nested ones make the
        // artifact ask by name, with no git command run.
        opts.collect_facts = false;
        opts.context = self.context.clone();
        let result = scan(&opts).map_err(|e| format!("the scan of the copy failed: {e}"))?;
        let path = qpath.to_string_lossy();
        let node = result
            .nodes
            .iter()
            .find(|n| n.path == path)
            .ok_or_else(|| "the copy is no longer classified as an artifact".to_string())?;
        verdict(node, rule_id)
    }
}

/// The scan's node of the copy, as a yes or no with the reason.
fn verdict(node: &Node, rule_id: &str) -> Result<(), String> {
    if !matches!(node.class, Class::Regenerable | Class::Cache) {
        return Err(format!("it is now classified {}", node.class.as_str()));
    }
    if node.rule_id.as_deref() != Some(rule_id) || !node.rule_also.is_empty() {
        return Err("another rule also matches it now".to_string());
    }
    if let Some(why) = &node.ask_reason {
        return Err(format!("it now asks: {why}"));
    }
    if node.contains_outputs {
        return Err("it now holds output files".to_string());
    }
    if node.unreadable > 0 {
        return Err(format!("{} entries could not be read", node.unreadable));
    }
    if node.skipped_mounts > 0 {
        return Err("a directory inside is on another file system".to_string());
    }
    if node.agent_files != Some(0) {
        return Err("a file inside looks like agent data".to_string());
    }
    Ok(())
}
