//! Recursive delete. Symlinks are removed, never followed.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use super::copy::Cancelled;
use crate::fs::{self, Backend, Kind};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

fn is_real_dir(b: &dyn Backend, path: &Path) -> Result<bool> {
    let e = b.stat(path);
    // A dangling symlink fails stat on some backends; it's still removable.
    Ok(matches!(e, Ok(e) if e.kind == Kind::Dir && !e.is_symlink))
}

/// Counts what deleting `path` would remove (not including `path` itself
/// when it is a directory).
pub fn summarize(b: &dyn Backend, path: &Path, cancel: &AtomicBool) -> Result<Summary> {
    let mut s = Summary::default();
    if is_real_dir(b, path)? {
        count(b, path, cancel, &mut s)?;
    } else {
        s.files = 1;
        s.bytes = b.stat(path).map(|e| e.size).unwrap_or(0);
    }
    Ok(s)
}

fn count(b: &dyn Backend, dir: &Path, cancel: &AtomicBool, s: &mut Summary) -> Result<()> {
    for c in b.list(dir)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        if c.kind == Kind::Dir && !c.is_symlink {
            s.dirs += 1;
            count(b, &fs::join(dir, &c.name), cancel, s)?;
        } else {
            s.files += 1;
            s.bytes += c.size;
        }
    }
    Ok(())
}

/// Deletes `path` bottom-up. `on_item` is called after each removal.
pub fn delete(b: &dyn Backend, path: &Path, cancel: &AtomicBool, on_item: &mut dyn FnMut(&str)) -> Result<()> {
    if is_real_dir(b, path)? {
        delete_dir(b, path, cancel, on_item)
    } else {
        b.remove_file(path)?;
        on_item(&fs::file_name(path));
        Ok(())
    }
}

fn delete_dir(b: &dyn Backend, dir: &Path, cancel: &AtomicBool, on_item: &mut dyn FnMut(&str)) -> Result<()> {
    for c in b.list(dir)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let p = fs::join(dir, &c.name);
        if c.kind == Kind::Dir && !c.is_symlink {
            delete_dir(b, &p, cancel, on_item)?;
        } else {
            b.remove_file(&p)?;
            on_item(&c.name);
        }
    }
    b.remove_dir(dir)?;
    on_item(&fs::file_name(dir));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::local::LocalFs;
    use std::fs as stdfs;

    #[test]
    fn deletes_tree_without_following_links() {
        let keep = tempfile::tempdir().unwrap();
        stdfs::write(keep.path().join("precious"), "x").unwrap();
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("t");
        stdfs::create_dir_all(root.join("a/b")).unwrap();
        stdfs::write(root.join("a/b/f"), "12345").unwrap();
        stdfs::write(root.join("g"), "1").unwrap();
        std::os::unix::fs::symlink(keep.path(), root.join("link")).unwrap();

        let cancel = AtomicBool::new(false);
        let s = summarize(&LocalFs, &root, &cancel).unwrap();
        // f, g and the symlink are files; a and a/b are dirs.
        assert_eq!((s.files, s.dirs), (3, 2));
        let mut n = 0;
        delete(&LocalFs, &root, &cancel, &mut |_| n += 1).unwrap();
        assert!(!root.exists());
        assert_eq!(n, 6);
        assert!(keep.path().join("precious").exists());
    }
}
