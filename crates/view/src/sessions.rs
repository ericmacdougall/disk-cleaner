//! Agent session folders: a display-only tag for well-known places where coding agents keep
//! transcripts and logs (see `rules/agent-sessions.toml`). The tag never feeds the plan; these
//! folders have no plan action yet. A session folder inside a folder that has a plan item shows
//! that item's action and path.

use crate::sanitize::display_name;
use crate::tree::{EntryKind, NodeId, Tree};
use disk_cleaner_core::PlanItem;
use serde::Serialize;
use std::path::Path;

/// The built-in layout list.
pub const BUILTIN_RULES: &str = include_str!("../rules/agent-sessions.toml");

/// One layout: a name and the folder's last path components, outermost first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub name: String,
    pub path: Vec<String>,
}

/// Parse a layout list.
pub fn parse_rules(text: &str) -> Result<Vec<Layout>, String> {
    let doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let Some(item) = doc.get("layout") else {
        return Ok(Vec::new());
    };
    let tables = item
        .as_array_of_tables()
        .ok_or("`layout` must be an array of tables")?;
    let mut out = Vec::new();
    for (i, t) in tables.iter().enumerate() {
        let name = t
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("layout {i}: `name` must be a string"))?;
        let parts = t
            .get("path")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("layout {i}: `path` must be an array"))?;
        let mut path = Vec::new();
        for p in parts.iter() {
            let s = p
                .as_str()
                .filter(|s| !s.is_empty() && !s.contains(['/', '\\']))
                .ok_or_else(|| format!("layout {i}: each path part must be one folder name"))?;
            path.push(s.to_string());
        }
        if path.is_empty() {
            return Err(format!("layout {i}: `path` is empty"));
        }
        out.push(Layout {
            name: name.to_string(),
            path,
        });
    }
    Ok(out)
}

/// The built-in layouts.
pub fn builtin() -> Vec<Layout> {
    parse_rules(BUILTIN_RULES).expect("the built-in agent session list parses")
}

/// The plan item that encloses a session folder, as shown next to it.
#[derive(Debug, Clone, Serialize)]
pub struct Enclosing {
    pub action: String,
    pub path: String,
}

/// One session folder found in the size walk.
#[derive(Debug, Clone, Serialize)]
pub struct SessionFolder {
    pub id: NodeId,
    /// The layout's name.
    pub layout: String,
    /// Display-safe path.
    pub path: String,
    pub alloc: u64,
    pub size: u64,
    pub files: u64,
    pub newest_mtime: Option<i64>,
    /// Always "no plan action yet": the tag is display only.
    pub plan_note: &'static str,
    pub enclosing: Option<Enclosing>,
}

/// The note every session folder carries.
pub const NO_PLAN_ACTION: &str = "no plan action yet";

fn matches(tree: &Tree, id: NodeId, layout: &Layout) -> bool {
    let mut cur = Some(id);
    for part in layout.path.iter().rev() {
        match cur {
            Some(c) if tree.name(c) == part.as_str() => cur = tree.parent(c),
            _ => return false,
        }
    }
    true
}

/// The deepest plan item whose folder holds `path`.
pub fn enclosing_item<'a>(items: &'a [PlanItem], path: &Path) -> Option<&'a PlanItem> {
    items
        .iter()
        .filter(|it| path.starts_with(Path::new(&it.path)))
        .max_by_key(|it| it.path.len())
}

/// Find the session folders in a size-walk tree. `items` are the plan's items, when a plan
/// exists, to show the enclosing action.
pub fn find(tree: &Tree, layouts: &[Layout], items: &[PlanItem]) -> Vec<SessionFolder> {
    let mut out = Vec::new();
    for id in 0..tree.len() as NodeId {
        if tree.kind(id) != EntryKind::Dir {
            continue;
        }
        let name = tree.name(id);
        let Some(layout) = layouts
            .iter()
            .find(|l| l.path.last().is_some_and(|last| last == name) && matches(tree, id, l))
        else {
            continue;
        };
        let real = tree.path(id);
        let enclosing = enclosing_item(items, &real).map(|it| Enclosing {
            action: it.action.as_str().to_string(),
            path: display_name(&it.path),
        });
        out.push(SessionFolder {
            id,
            layout: layout.name.clone(),
            path: display_name(&real.to_string_lossy()),
            alloc: tree.alloc(id),
            size: tree.size(id),
            files: tree.files(id),
            newest_mtime: tree.mtime(id),
            plan_note: NO_PLAN_ACTION,
            enclosing,
        });
    }
    out.sort_by(|a, b| b.alloc.cmp(&a.alloc).then_with(|| a.path.cmp(&b.path)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{NewEntry, TreeBuilder, WalkStats};
    use disk_cleaner_core::{Action, Class, ProjectState};

    fn dir(b: &mut TreeBuilder, parent: Option<NodeId>, name: &str) -> NodeId {
        b.push(NewEntry {
            parent,
            name,
            kind: EntryKind::Dir,
            size: 0,
            alloc: 0,
            files: 0,
            mtime: 100,
            flags: 0,
        })
        .unwrap()
    }

    fn file(b: &mut TreeBuilder, parent: NodeId, name: &str, alloc: u64) {
        b.push(NewEntry {
            parent: Some(parent),
            name,
            kind: EntryKind::File,
            size: alloc,
            alloc,
            files: 0,
            mtime: 200,
            flags: 0,
        })
        .unwrap();
    }

    fn item(path: &str, action: Action) -> PlanItem {
        PlanItem {
            node: 0,
            path: path.into(),
            class: Class::Other,
            action,
            state: ProjectState::Idle,
            state_reason: String::new(),
            reason: String::new(),
            reclaimable_bytes: 0,
            bytes_total: 0,
            days_since_activity: None,
            facts: None,
            preserve: None,
        }
    }

    #[test]
    fn the_builtin_list_parses() {
        let l = builtin();
        assert!(l.iter().any(|x| x.path == vec![".claude", "projects"]));
        assert!(l.iter().any(|x| x.path == vec![".codex", "sessions"]));
    }

    #[test]
    fn bad_rules_are_errors() {
        assert!(parse_rules("[[layout]]\nname = \"x\"\npath = []\n").is_err());
        assert!(parse_rules("[[layout]]\nname = \"x\"\npath = [\"a/b\"]\n").is_err());
        assert!(parse_rules("[[layout]]\npath = [\"a\"]\n").is_err());
        assert!(parse_rules("not toml [").is_err());
    }

    #[test]
    fn finds_layouts_and_shows_the_enclosing_plan_item() {
        let mut b = TreeBuilder::new(true);
        let root = dir(&mut b, None, "/home-synthetic");
        let claude = dir(&mut b, Some(root), ".claude");
        let projects = dir(&mut b, Some(claude), "projects");
        file(&mut b, projects, "s1.jsonl", 4096);
        file(&mut b, projects, "s2.jsonl", 8192);
        // A folder named `projects` elsewhere is not a session folder.
        let other = dir(&mut b, Some(root), "work");
        let p2 = dir(&mut b, Some(other), "projects");
        file(&mut b, p2, "a.txt", 4096);
        let tree = b.finish(Vec::new(), WalkStats::default());
        let items = vec![
            item("/home-synthetic", Action::Keep),
            item("/home-synthetic/.claude", Action::Ask),
        ];
        let found = find(&tree, &builtin(), &items);
        assert_eq!(found.len(), 1);
        let s = &found[0];
        assert_eq!(s.layout, "Claude Code projects");
        assert_eq!(s.alloc, 12288);
        assert_eq!(s.files, 2);
        assert_eq!(s.plan_note, "no plan action yet");
        let enc = s.enclosing.as_ref().unwrap();
        assert_eq!(enc.action, "ask");
        assert_eq!(enc.path, "/home-synthetic/.claude");
        assert!(find(&tree, &builtin(), &[])[0].enclosing.is_none());
    }
}
