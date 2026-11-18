//! Filesystem backends. Both panes talk to a `Backend`, so copy, delete,
//! preview and search work the same for local and remote locations.

use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use anyhow::Result;

pub mod local;
pub mod sftp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Other,
}

/// One directory entry. `kind`, `size` and `modified` describe the symlink
/// target when the entry is a symlink that resolves; `is_symlink` records
/// that the entry itself is a link.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub mode: Option<u32>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WalkOpts {
    pub hidden: bool,
    pub respect_ignore: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CaseMode {
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Clone, Debug, Default)]
pub struct GrepQuery {
    pub pattern: String,
    pub regex: bool,
    pub case: CaseMode,
    pub whole_word: bool,
    pub hidden: bool,
    pub respect_ignore: bool,
    /// rg-style globs: `*.rs` includes, `!target/**` excludes.
    pub globs: Vec<String>,
}

/// Results streamed from `walk_files` / `grep`. Paths are relative to the
/// search root and always use `/` separators.
#[derive(Clone, Debug)]
pub enum SearchEvent {
    Path(String),
    Match {
        path: String,
        line_no: u64,
        line: String,
        /// Byte ranges of the matches within `line`.
        ranges: Vec<Range<usize>>,
    },
    Error(String),
}

pub type Emit<'a> = &'a (dyn Fn(SearchEvent) + Send + Sync);

/// Must be safe to use from several threads at once: the UI thread lists
/// directories while the worker copies and search threads walk or grep.
pub trait Backend: Send + Sync {
    /// "Local" or "user@host".
    fn label(&self) -> String;
    fn is_remote(&self) -> bool;
    fn canonicalize(&self, path: &Path) -> Result<PathBuf>;
    /// Directory contents without `.` and `..`.
    fn list(&self, dir: &Path) -> Result<Vec<Entry>>;
    /// Follows symlinks; `is_symlink` is still filled in from lstat.
    fn stat(&self, path: &Path) -> Result<Entry>;
    /// True if anything (including a dangling symlink) exists at `path`.
    fn exists(&self, path: &Path) -> Result<bool>;
    fn open_read(&self, path: &Path) -> Result<Box<dyn Read + Send>>;
    /// Create or truncate.
    fn open_write(&self, path: &Path) -> Result<Box<dyn Write + Send>>;
    fn mkdir(&self, path: &Path) -> Result<()>;
    /// Removes a file or a symlink (never follows it).
    fn remove_file(&self, path: &Path) -> Result<()>;
    /// Removes an empty directory.
    fn remove_dir(&self, path: &Path) -> Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    fn set_mode(&self, path: &Path, mode: u32) -> Result<()>;
    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> Result<()>;

    /// Stream every file under `root` as `SearchEvent::Path` until done or
    /// `cancel` is set.
    fn walk_files(&self, root: &Path, opts: WalkOpts, emit: Emit, cancel: &AtomicBool) -> Result<()>;
    /// Stream matching lines under `root` as `SearchEvent::Match`.
    fn grep(&self, root: &Path, query: &GrepQuery, emit: Emit, cancel: &AtomicBool) -> Result<()>;

    /// Re-establish a dropped connection. No-op for local.
    fn reconnect(&self) -> Result<()> {
        Ok(())
    }
}

/// Joins remote-style paths with `/` regardless of host platform.
pub fn join(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

pub fn parent(path: &Path) -> Option<PathBuf> {
    path.parent().map(Path::to_path_buf)
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}
