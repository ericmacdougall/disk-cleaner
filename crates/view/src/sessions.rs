//! Agent session folders: a display-only tag for well-known places where coding agents keep
//! transcripts and logs (see `rules/agent-sessions.toml`). The tag never feeds the plan; these
//! folders have no plan action yet. A session folder inside a folder that has a plan item shows
//! that item's action and path.

use crate::sanitize::display_name;
use crate::tree::{EntryKind, NodeId, Tree};
use disk_cleaner_core::PlanItem;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// The real path, to match plan items (never sent to the screen).
    #[serde(skip)]
    pub real: PathBuf,
}

/// The note every session folder carries.
pub const NO_PLAN_ACTION: &str = "no plan action yet";

/// Whether `id`'s path ends with the layout's components. A root's name is the whole path the
/// walk started from, so the remaining components are compared with its trailing components.
fn ends_with_layout(tree: &Tree, id: NodeId, layout: &Layout) -> bool {
    let mut cur = id;
    let mut parts = layout.path.iter().rev();
    loop {
        let Some(part) = parts.next() else {
            return true;
        };
        let Some(parent) = tree.parent(cur) else {
            let mut tail = Path::new(tree.name(cur)).components().rev();
            return std::iter::once(part).chain(parts).all(
                |p| matches!(tail.next(), Some(std::path::Component::Normal(c)) if c == p.as_str()),
            );
        };
        if tree.name(cur) != part.as_str() {
            return false;
        }
        cur = parent;
    }
}

/// The deepest plan item whose folder holds `path`.
pub fn enclosing_item<'a>(items: &'a [PlanItem], path: &Path) -> Option<&'a PlanItem> {
    items
        .iter()
        .filter(|it| path.starts_with(Path::new(&it.path)))
        .max_by_key(|it| it.path.len())
}

/// Set each folder's enclosing plan item (none without items). The items are looked up by
/// each folder's ancestors, deepest first, so the cost is O(items + folders × depth).
pub fn set_enclosing(folders: &mut [SessionFolder], items: &[PlanItem]) {
    let by_path: HashMap<&Path, &PlanItem> = items
        .iter()
        .map(|it| (Path::new(it.path.as_str()), it))
        .collect();
    for f in folders.iter_mut() {
        f.enclosing = f
            .real
            .ancestors()
            .find_map(|a| by_path.get(a))
            .map(|it| Enclosing {
                action: it.action.as_str().to_string(),
                path: display_name(&it.path),
            });
    }
}

/// Find the session folders in a size-walk tree. `items` are the plan's items, when a plan
/// exists, to show the enclosing action.
pub fn find(tree: &Tree, layouts: &[Layout], items: &[PlanItem]) -> Vec<SessionFolder> {
    let mut out = Vec::new();
    for id in 0..tree.len() as NodeId {
        if tree.kind(id) != EntryKind::Dir {
            continue;
        }
        let Some(layout) = layouts.iter().find(|l| ends_with_layout(tree, id, l)) else {
            continue;
        };
        let real = tree.path(id);
        out.push(SessionFolder {
            id,
            layout: layout.name.clone(),
            path: display_name(&real.to_string_lossy()),
            alloc: tree.alloc(id),
            size: tree.size(id),
            files: tree.files(id),
            newest_mtime: tree.mtime(id),
            plan_note: NO_PLAN_ACTION,
            enclosing: None,
            real,
        });
    }
    out.sort_by(|a, b| b.alloc.cmp(&a.alloc).then_with(|| a.path.cmp(&b.path)));
    set_enclosing(&mut out, items);
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

    /// The ancestor lookup picks the same item as the linear search: the deepest folder that
    /// holds the path, never a mere string prefix.
    #[test]
    fn set_enclosing_agrees_with_the_linear_search() {
        let mut b = TreeBuilder::new(true);
        let root = dir(&mut b, None, "/h");
        let claude = dir(&mut b, Some(root), ".claude");
        let projects = dir(&mut b, Some(claude), "projects");
        file(&mut b, projects, "s.jsonl", 4096);
        let tree = b.finish(Vec::new(), WalkStats::default());
        let cases: [Vec<PlanItem>; 6] = [
            vec![],
            vec![item("/h/.cla", Action::RemoveRegenerable)],
            vec![item("/h", Action::Keep), item("/h/.claude", Action::Ask)],
            vec![item("/h/.claude", Action::Ask), item("/h", Action::Keep)],
            vec![item("/h/.claude/projects", Action::Keep)],
            vec![item("/other", Action::Keep)],
        ];
        for (i, items) in cases.iter().enumerate() {
            let found = find(&tree, &builtin(), items);
            let real = found[0].real.clone();
            let want = enclosing_item(items, &real).map(|it| it.path.clone());
            let got = found[0].enclosing.as_ref().map(|e| e.path.clone());
            assert_eq!(got, want, "case {i}");
        }
    }

    /// A walk started at the session folder itself, or at its parent, names the root by its
    /// whole path; the layout must still match by the path's trailing components.
    #[test]
    fn a_root_named_by_its_full_path_matches_by_trailing_components() {
        let sep = std::path::MAIN_SEPARATOR;
        let base = if cfg!(windows) { "C:\\syn" } else { "/syn" };
        let mut b = TreeBuilder::new(true);
        let at_folder = dir(&mut b, None, &format!("{base}{sep}.codex{sep}sessions"));
        file(&mut b, at_folder, "r.jsonl", 4096);
        let at_parent = dir(&mut b, None, &format!("{base}{sep}u{sep}.claude"));
        let projects = dir(&mut b, Some(at_parent), "projects");
        file(&mut b, projects, "s.jsonl", 8192);
        // Only one trailing component agrees: not a match.
        let near = dir(&mut b, None, &format!("{base}{sep}other{sep}sessions"));
        file(&mut b, near, "x.txt", 4096);
        // A path shorter than the layout: not a match.
        let short = dir(&mut b, None, "sessions");
        file(&mut b, short, "y.txt", 4096);
        let tree = b.finish(Vec::new(), WalkStats::default());
        let mut found: Vec<String> = find(&tree, &builtin(), &[])
            .into_iter()
            .map(|s| s.layout)
            .collect();
        found.sort();
        let mut want: Vec<String> = builtin()
            .into_iter()
            .filter(|l| {
                l.path == vec![".codex", "sessions"] || l.path == vec![".claude", "projects"]
            })
            .map(|l| l.name)
            .collect();
        want.sort();
        assert_eq!(found, want);
    }
}
