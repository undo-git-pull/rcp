use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::fs::{self, Backend, Entry, Kind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Size,
    Modified,
}

impl SortBy {
    pub fn next(self) -> SortBy {
        match self {
            SortBy::Name => SortBy::Size,
            SortBy::Size => SortBy::Modified,
            SortBy::Modified => SortBy::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortBy::Name => "name",
            SortBy::Size => "size",
            SortBy::Modified => "time",
        }
    }
}

pub struct Pane {
    pub backend: Arc<dyn Backend>,
    pub cwd: PathBuf,
    /// Everything in `cwd` as last listed.
    all: Vec<Entry>,
    /// What's shown: `..` first, then filtered and sorted entries.
    pub items: Vec<Entry>,
    pub cursor: usize,
    pub offset: usize,
    pub marked: HashSet<String>,
    pub show_hidden: bool,
    pub sort: SortBy,
    pub loading: bool,
    pub error: Option<String>,
    /// Bumped per listing request so stale results can be dropped.
    pub list_id: u64,
    /// Name to put the cursor on when the next listing arrives.
    pub select_after_load: Option<String>,
}

fn parent_entry() -> Entry {
    Entry { name: "..".into(), kind: Kind::Dir, is_symlink: false, size: 0, modified: None, mode: None }
}

impl Pane {
    pub fn new(backend: Arc<dyn Backend>, cwd: PathBuf, show_hidden: bool) -> Pane {
        Pane {
            backend,
            cwd,
            all: vec![],
            items: vec![],
            cursor: 0,
            offset: 0,
            marked: HashSet::new(),
            show_hidden,
            sort: SortBy::Name,
            loading: false,
            error: None,
            list_id: 0,
            select_after_load: None,
        }
    }

    pub fn title(&self) -> String {
        format!("{}: {}", self.backend.label(), self.cwd.display())
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.items.get(self.cursor)
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected().filter(|e| e.name != "..").map(|e| fs::join(&self.cwd, &e.name))
    }

    /// Marked entries if any, otherwise the one under the cursor.
    pub fn targets(&self) -> Vec<PathBuf> {
        if self.marked.is_empty() {
            return self.selected_path().into_iter().collect();
        }
        let mut names: Vec<&String> = self.marked.iter().collect();
        names.sort();
        names.into_iter().map(|n| fs::join(&self.cwd, n)).collect()
    }

    pub fn set_entries(&mut self, cwd: PathBuf, entries: Vec<Entry>) {
        let changed_dir = cwd != self.cwd;
        let keep = self.select_after_load.take().or_else(|| self.selected().map(|e| e.name.clone()));
        if changed_dir {
            self.marked.clear();
            self.offset = 0;
        }
        self.cwd = cwd;
        self.all = entries;
        self.loading = false;
        self.error = None;
        self.rebuild();
        self.marked.retain(|m| self.all.iter().any(|e| &e.name == m));
        self.cursor = keep.and_then(|n| self.items.iter().position(|e| e.name == n)).unwrap_or(0);
    }

    pub fn rebuild(&mut self) {
        let keep = self.selected().map(|e| e.name.clone());
        let mut items: Vec<Entry> =
            self.all.iter().filter(|e| self.show_hidden || !e.name.starts_with('.')).cloned().collect();
        let sort = self.sort;
        items.sort_by(|a, b| {
            b.is_dir().cmp(&a.is_dir()).then_with(|| match sort {
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortBy::Size => b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)),
                SortBy::Modified => b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)),
            })
        });
        if self.cwd.parent().is_some() {
            items.insert(0, parent_entry());
        }
        self.items = items;
        self.cursor = keep.and_then(|n| self.items.iter().position(|e| e.name == n)).unwrap_or(0);
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let max = self.items.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
    }

    pub fn toggle_mark(&mut self) {
        if let Some(e) = self.selected().filter(|e| e.name != "..") {
            let name = e.name.clone();
            if !self.marked.remove(&name) {
                self.marked.insert(name);
            }
        }
        self.move_cursor(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::local::LocalFs;

    fn entry(name: &str, kind: Kind, size: u64) -> Entry {
        Entry { name: name.into(), kind, is_symlink: false, size, modified: None, mode: None }
    }

    #[test]
    fn sorts_dirs_first_hides_dotfiles_keeps_cursor() {
        let mut p = Pane::new(Arc::new(LocalFs), "/x".into(), false);
        p.set_entries(
            "/x".into(),
            vec![entry("b.txt", Kind::File, 5), entry(".h", Kind::File, 1), entry("Z", Kind::Dir, 0), entry("a.txt", Kind::File, 9)],
        );
        let names: Vec<_> = p.items.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["..", "Z", "a.txt", "b.txt"]);
        p.cursor = 3;
        p.sort = SortBy::Size;
        p.rebuild();
        assert_eq!(p.selected().unwrap().name, "b.txt");
        assert_eq!(p.items[2].name, "a.txt");
        p.show_hidden = true;
        p.rebuild();
        assert!(p.items.iter().any(|e| e.name == ".h"));
    }

    #[test]
    fn targets_prefer_marks() {
        let mut p = Pane::new(Arc::new(LocalFs), "/x".into(), false);
        p.set_entries("/x".into(), vec![entry("a", Kind::File, 1), entry("b", Kind::File, 1)]);
        p.cursor = 1;
        assert_eq!(p.targets(), vec![PathBuf::from("/x/a")]);
        p.toggle_mark();
        p.toggle_mark();
        assert_eq!(p.targets(), vec![PathBuf::from("/x/a"), PathBuf::from("/x/b")]);
        p.cursor = 0;
        p.toggle_mark(); // ".." can't be marked
        assert_eq!(p.marked.len(), 2);
    }
}
