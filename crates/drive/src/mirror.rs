//! What the Kimono folder on this computer should hold, kept in step with Drive. Pure: it only
//! decides what to create, move, replace or remove; each OS's backend does it.
//!
//! The folder holds one folder per space, plus "Shared with you" for things shared from spaces
//! the person isn't in. Names that a desktop can't hold are made safe; two that end up alike
//! get numbered.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use kimono_core::{Node, Space};
use serde::{Deserialize, Serialize};

pub const SHARED: &str = "shared-with-you";
pub const SHARED_NAME: &str = "Shared with you";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// The local parent: None at the top of the Kimono folder.
    pub parent: Option<String>,
    /// The name as it is on disk here.
    pub name: String,
    pub folder: bool,
    pub size: u64,
    pub content_version: i64,
    pub version: i64,
    /// Milliseconds since 1970.
    pub created: i64,
    pub updated: i64,
    pub can_edit: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Create { path: PathBuf, entry: Entry },
    Move { from: PathBuf, to: PathBuf, entry: Entry },
    /// Its bytes changed on Kimono: the copy here is stale.
    Replace { path: PathBuf, entry: Entry },
    Remove { path: PathBuf, folder: bool },
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Mirror {
    pub entries: HashMap<String, Entry>,
    pub cursor: Option<i64>,
    #[serde(skip)]
    children: HashMap<Option<String>, Vec<String>>,
}

/// A name Windows (and macOS, and Linux) can hold. Characters Windows refuses become their
/// full-width lookalikes, so the name still reads the same.
pub fn safe_name(name: &str) -> String {
    let mut out: String = name.chars().map(|c| match c {
        '<' => '＜', '>' => '＞', ':' => '：', '"' => '＂', '/' => '／', '\\' => '＼', '|' => '｜', '?' => '？', '*' => '＊',
        c if (c as u32) < 32 => '_',
        c => c,
    }).collect();
    while out.ends_with(' ') || out.ends_with('.') {
        let last = out.pop().unwrap();
        out.push(if last == '.' { '．' } else { '_' });
        break;
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"];
    if RESERVED.contains(&stem.as_str()) { out.insert(0, '_'); }
    if out.is_empty() { out.push('_'); }
    out
}

fn numbered(name: &str, n: usize) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

impl Entry {
    pub fn from_node(node: &Node, parent: Option<String>) -> Self {
        Self {
            id: node.id.clone(),
            parent,
            name: safe_name(&node.name),
            folder: node.is_folder(),
            size: node.size,
            content_version: node.content_version,
            version: node.version,
            created: node.created_at as i64,
            updated: node.updated_at as i64,
            can_edit: node.can_edit(),
        }
    }

    pub fn for_space(space: &Space) -> Self {
        Self { id: space.root_id.clone(), parent: None, name: safe_name(&space.name), folder: true, size: 0, content_version: 0, version: 0, created: 0, updated: 0, can_edit: space.can_edit() }
    }

    pub fn shared() -> Self {
        Self { id: SHARED.into(), parent: None, name: SHARED_NAME.into(), folder: true, size: 0, content_version: 0, version: 0, created: 0, updated: 0, can_edit: false }
    }
}

/// Where a node from Drive sits here: under its folder, or Shared with you.
pub fn local_parent(node: &Node) -> Option<String> {
    if node.is_shared_with_you() { Some(SHARED.into()) } else { node.parent_id.clone() }
}

impl Mirror {
    pub fn load(bytes: &[u8]) -> Self {
        let mut m: Mirror = serde_json::from_slice(bytes).unwrap_or_default();
        m.reindex();
        m
    }

    pub fn save(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    fn reindex(&mut self) {
        self.children.clear();
        for e in self.entries.values() { self.children.entry(e.parent.clone()).or_default().push(e.id.clone()); }
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.get(id)
    }

    pub fn children(&self, id: Option<&str>) -> impl Iterator<Item = &Entry> {
        self.children.get(&id.map(String::from)).into_iter().flatten().filter_map(|c| self.entries.get(c))
    }

    /// Where an entry is, relative to the Kimono folder. None when it (or a folder above it) isn't here.
    pub fn path(&self, id: &str) -> Option<PathBuf> {
        let mut parts = Vec::new();
        let mut at = self.entries.get(id)?;
        loop {
            parts.push(at.name.clone());
            match &at.parent {
                None => break,
                Some(p) => at = self.entries.get(p)?,
            }
        }
        Some(parts.iter().rev().collect())
    }

    /// The entry at a path relative to the Kimono folder, matching names as Windows does (any case).
    pub fn at(&self, path: &std::path::Path) -> Option<&Entry> {
        let mut parent: Option<String> = None;
        let mut found = None;
        for part in path.iter() {
            let part = part.to_string_lossy();
            let e = self.children(parent.as_deref()).find(|e| e.name.eq_ignore_ascii_case(&part))?;
            parent = Some(e.id.clone());
            found = Some(e);
        }
        found
    }

    /// A name for `wanted` in `parent` that nothing else there has.
    fn free_name(&self, parent: Option<&str>, wanted: &str, except: &str) -> String {
        let taken: HashSet<String> = self.children(parent).filter(|e| e.id != except).map(|e| e.name.to_lowercase()).collect();
        if !taken.contains(&wanted.to_lowercase()) { return wanted.into(); }
        (2..).map(|n| numbered(wanted, n)).find(|n| !taken.contains(&n.to_lowercase())).unwrap()
    }

    fn insert(&mut self, entry: Entry) {
        if let Some(old) = self.entries.get(&entry.id) {
            let old_parent = old.parent.clone();
            if let Some(list) = self.children.get_mut(&old_parent) { list.retain(|c| *c != entry.id); }
        }
        self.children.entry(entry.parent.clone()).or_default().push(entry.id.clone());
        self.entries.insert(entry.id.clone(), entry);
    }

    fn forget(&mut self, id: &str) -> Vec<Entry> {
        let mut gone = Vec::new();
        let mut stack = vec![id.to_string()];
        while let Some(next) = stack.pop() {
            if let Some(e) = self.entries.remove(&next) {
                if let Some(list) = self.children.get_mut(&e.parent) { list.retain(|c| *c != next); }
                stack.extend(self.children.remove(&Some(next.clone())).unwrap_or_default());
                gone.push(e);
            }
        }
        gone
    }

    /// Takes one entry as Drive has it now, saying what to do here to match.
    fn take(&mut self, mut wanted: Entry, actions: &mut Vec<Action>) {
        if let Some(p) = &wanted.parent { if !self.entries.contains_key(p) { return; } }
        wanted.name = self.free_name(wanted.parent.as_deref(), &wanted.name, &wanted.id);
        match self.entries.get(&wanted.id).cloned() {
            None => {
                self.insert(wanted.clone());
                actions.push(Action::Create { path: self.path(&wanted.id).unwrap(), entry: wanted });
            }
            Some(old) => {
                let from = self.path(&old.id);
                let moved = old.parent != wanted.parent || old.name != wanted.name;
                let replaced = !old.folder && old.content_version != wanted.content_version;
                self.insert(wanted.clone());
                let to = self.path(&wanted.id).unwrap();
                if moved { if let Some(from) = from { actions.push(Action::Move { from, to: to.clone(), entry: wanted.clone() }); } }
                if replaced { actions.push(Action::Replace { path: to, entry: wanted }); }
            }
        }
    }

    fn drop_entry(&mut self, id: &str, actions: &mut Vec<Action>) {
        let Some(path) = self.path(id) else { return };
        let folder = self.entries.get(id).is_some_and(|e| e.folder);
        self.forget(id);
        actions.push(Action::Remove { path, folder });
    }

    /// Everything Drive has, from a full listing: what to do here, parents before children.
    pub fn reconcile(&mut self, spaces: &[Space], nodes: &[Node], shared: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut wanted: HashSet<String> = HashSet::new();
        for s in spaces { wanted.insert(s.root_id.clone()); }
        if shared { wanted.insert(SHARED.into()); }
        for n in nodes { wanted.insert(n.id.clone()); }
        let stale: Vec<String> = self.entries.keys().filter(|id| !wanted.contains(*id)).cloned().collect();
        for id in stale { if self.entries.contains_key(&id) { self.drop_entry(&id, &mut actions); } }
        actions.extend(self.apply_spaces(spaces, shared));
        actions.extend(self.apply_nodes(nodes));
        actions
    }

    fn apply_spaces(&mut self, spaces: &[Space], shared: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        let roots: HashSet<&str> = spaces.iter().map(|s| s.root_id.as_str()).collect();
        let gone: Vec<String> = self.children(None).filter(|e| e.id != SHARED && !roots.contains(e.id.as_str())).map(|e| e.id.clone()).collect();
        for id in gone { self.drop_entry(&id, &mut actions); }
        for s in spaces { self.take(Entry::for_space(s), &mut actions); }
        if shared { self.take(Entry::shared(), &mut actions); }
        actions
    }

    /// Nodes in any order: each waits until its folder is here.
    fn apply_nodes(&mut self, nodes: &[Node]) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut waiting: Vec<&Node> = nodes.iter().filter(|n| n.parent_id.is_some() || n.is_shared_with_you()).collect();
        loop {
            let before = waiting.len();
            waiting.retain(|n| {
                let parent = local_parent(n);
                if parent.as_ref().is_some_and(|p| !self.entries.contains_key(p)) && !waiting_has(nodes, parent.as_deref()) { return false; }
                if parent.as_ref().is_some_and(|p| !self.entries.contains_key(p)) { return true; }
                self.take(Entry::from_node(n, parent), &mut actions);
                false
            });
            if waiting.is_empty() || waiting.len() == before { break; }
        }
        actions
    }

    /// What changed on Drive since the cursor.
    pub fn apply_changes(&mut self, changed: &[Node], removed: &[String], spaces: &[Space]) -> Vec<Action> {
        let shared = self.entries.contains_key(SHARED) || changed.iter().any(|n| n.is_shared_with_you());
        let mut actions = self.apply_spaces(spaces, shared);
        for id in removed { self.drop_entry(id, &mut actions); }
        actions.extend(self.apply_nodes(changed));
        actions
    }

    // ---- What happened here, done on Drive already ----

    pub fn added_here(&mut self, entry: Entry) {
        self.insert(entry);
    }

    pub fn moved_here(&mut self, id: &str, parent: Option<String>, name: &str) {
        if let Some(mut e) = self.entries.get(id).cloned() {
            e.parent = parent;
            e.name = name.into();
            self.insert(e);
        }
    }

    pub fn removed_here(&mut self, id: &str) {
        self.forget(id);
    }

    pub fn saved_here(&mut self, id: &str, size: u64, content_version: i64) {
        if let Some(e) = self.entries.get_mut(id) { e.size = size; e.content_version = content_version; }
    }
}

fn waiting_has(nodes: &[Node], id: Option<&str>) -> bool {
    id.is_some_and(|id| id == SHARED || nodes.iter().any(|n| n.id == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parent: &str, kind: &str, name: &str, cv: i64) -> Node {
        Node { id: id.into(), space_id: "s".into(), parent_id: Some(parent.into()), kind: kind.into(), name: name.into(), size: 3, mime: None,
            created_at: 0.0, updated_at: 0.0, version: 1, content_version: cv, role: Some("owner".into()), shared_with_you: None }
    }
    fn space() -> Space { Space { id: "s".into(), name: "Family".into(), root_id: "r".into(), role: "owner".into() } }

    #[test]
    fn lays_out_spaces_and_folders_parents_first() {
        let mut m = Mirror::default();
        let nodes = vec![node("f", "d", "file", "a.txt", 1), node("d", "r", "folder", "Docs", 1)];
        let actions = m.reconcile(&[space()], &nodes, false);
        let paths: Vec<_> = actions.iter().map(|a| match a { Action::Create { path, .. } => path.clone(), other => panic!("{other:?}") }).collect();
        assert_eq!(paths, vec![PathBuf::from("Family"), PathBuf::from("Family/Docs"), PathBuf::from("Family/Docs/a.txt")]);
        assert_eq!(m.at(std::path::Path::new("family/docs/A.TXT")).unwrap().id, "f");
    }

    #[test]
    fn follows_renames_edits_and_removals() {
        let mut m = Mirror::default();
        m.reconcile(&[space()], &[node("d", "r", "folder", "Docs", 1), node("f", "d", "file", "a.txt", 1)], false);
        let actions = m.apply_changes(&[node("d", "r", "folder", "Papers", 1), node("f", "d", "file", "a.txt", 2)], &[], &[space()]);
        assert_eq!(actions, vec![
            Action::Move { from: "Family/Docs".into(), to: "Family/Papers".into(), entry: m.get("d").unwrap().clone() },
            Action::Replace { path: "Family/Papers/a.txt".into(), entry: m.get("f").unwrap().clone() },
        ]);
        let actions = m.apply_changes(&[], &["d".into()], &[space()]);
        assert_eq!(actions, vec![Action::Remove { path: "Family/Papers".into(), folder: true }]);
        assert!(m.get("f").is_none());
    }

    #[test]
    fn makes_names_safe_and_apart() {
        assert_eq!(safe_name("a:b?.txt"), "a：b？.txt");
        assert_eq!(safe_name("con.txt"), "_con.txt");
        assert_eq!(safe_name("trailing."), "trailing．");
        let mut m = Mirror::default();
        m.reconcile(&[space()], &[node("a", "r", "file", "x:y", 1), node("b", "r", "file", "x：y", 1)], false);
        let mut names: Vec<_> = m.children(Some("r")).map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["x：y", "x：y (2)"]);
    }

    #[test]
    fn a_space_left_goes() {
        let mut m = Mirror::default();
        m.reconcile(&[space()], &[node("d", "r", "folder", "Docs", 1)], false);
        let actions = m.apply_changes(&[], &[], &[]);
        assert_eq!(actions, vec![Action::Remove { path: "Family".into(), folder: true }]);
        assert!(m.entries.is_empty());
    }

    #[test]
    fn survives_a_save_and_load() {
        let mut m = Mirror::default();
        m.reconcile(&[space()], &[node("d", "r", "folder", "Docs", 1)], false);
        m.cursor = Some(7);
        let back = Mirror::load(&m.save());
        assert_eq!(back.cursor, Some(7));
        assert_eq!(back.path("d").unwrap(), PathBuf::from("Family/Docs"));
    }
}
