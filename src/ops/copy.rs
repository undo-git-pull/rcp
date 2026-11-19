//! Streaming, recursive copy between any two backends.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};

use crate::fs::{self, Backend, Kind};

const CHUNK: usize = 256 * 1024;
pub const PART_SUFFIX: &str = ".rcp-part";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// For a directory: merge into it, asking again for clashing files.
    Overwrite,
    Skip,
    Rename,
    OverwriteAll,
    Cancel,
}

#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}

/// Bytes and file count under `path`, not following directory symlinks.
pub fn scan(b: &dyn Backend, path: &Path, cancel: &AtomicBool) -> Result<(u64, u64)> {
    let e = b.stat(path)?;
    match e.kind {
        Kind::Dir if e.is_symlink => Ok((0, 0)),
        Kind::Dir => {
            let (mut bytes, mut files) = (0, 0);
            for c in b.list(path)? {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Cancelled.into());
                }
                let p = fs::join(path, &c.name);
                match c.kind {
                    Kind::Dir if !c.is_symlink => {
                        let (bb, ff) = scan(b, &p, cancel)?;
                        bytes += bb;
                        files += ff;
                    }
                    Kind::File => {
                        bytes += c.size;
                        files += 1;
                    }
                    _ => {}
                }
            }
            Ok((bytes, files))
        }
        Kind::File => Ok((e.size, 1)),
        Kind::Other => Ok((0, 0)),
    }
}

/// `photo.jpg` -> `photo (1).jpg`, `dir` -> `dir (1)`, `.bashrc` -> `.bashrc (1)`.
pub fn numbered_name(name: &str, n: u32, is_dir: bool) -> String {
    let split = if is_dir {
        None
    } else {
        name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty())
    };
    match split {
        Some((stem, ext)) => format!("{stem} ({n}).{ext}"),
        None => format!("{name} ({n})"),
    }
}

fn unique_path(b: &dyn Backend, path: &Path, is_dir: bool) -> Result<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let name = fs::file_name(path);
    for n in 1..10_000 {
        let p = fs::join(dir, &numbered_name(&name, n, is_dir));
        if !b.exists(&p)? {
            return Ok(p);
        }
    }
    bail!("no free name for {}", path.display())
}

pub struct Copier<'a> {
    pub src: &'a dyn Backend,
    pub dst: &'a dyn Backend,
    pub cancel: &'a AtomicBool,
    pub on_progress: &'a mut dyn FnMut(u64, &str),
    pub ask: &'a mut dyn FnMut(&Path, bool) -> Resolution,
    pub overwrite_all: bool,
    /// Non-fatal problems (skipped symlinks, failed files inside a tree).
    pub notes: Vec<String>,
    pub done_bytes: u64,
}

impl Copier<'_> {
    fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    /// Copies `src_path` into `dst_dir`. Returns the name it ended up with,
    /// or None if skipped.
    pub fn copy_into(&mut self, src_path: &Path, dst_dir: &Path) -> Result<Option<String>> {
        let name = fs::file_name(src_path);
        let dst_path = fs::join(dst_dir, &name);
        self.copy_entry(src_path, &dst_path, true)
    }

    fn resolve(&mut self, dst_path: &Path, is_dir: bool) -> Result<Option<PathBuf>> {
        if self.overwrite_all && !is_dir {
            return Ok(Some(dst_path.to_path_buf()));
        }
        match (self.ask)(dst_path, is_dir) {
            Resolution::Overwrite => Ok(Some(dst_path.to_path_buf())),
            Resolution::OverwriteAll => {
                self.overwrite_all = true;
                Ok(Some(dst_path.to_path_buf()))
            }
            Resolution::Skip => Ok(None),
            Resolution::Rename => Ok(Some(unique_path(self.dst, dst_path, is_dir)?)),
            Resolution::Cancel => Err(Cancelled.into()),
        }
    }

    fn copy_entry(
        &mut self,
        src_path: &Path,
        dst_path: &Path,
        top: bool,
    ) -> Result<Option<String>> {
        self.check_cancel()?;
        let e = self.src.stat(src_path)?;
        match e.kind {
            Kind::Dir if e.is_symlink => {
                self.notes
                    .push(format!("skipped directory symlink {}", src_path.display()));
                Ok(None)
            }
            Kind::Dir => {
                let mut dst_path = dst_path.to_path_buf();
                if self.dst.exists(&dst_path)? {
                    if top {
                        match self.resolve(&dst_path, true)? {
                            Some(p) => dst_path = p,
                            None => return Ok(None),
                        }
                    }
                    if !self.dst.exists(&dst_path)? {
                        self.dst.mkdir(&dst_path)?;
                    } else if !self.dst.stat(&dst_path)?.is_dir() {
                        bail!("{} exists and is not a directory", dst_path.display());
                    }
                } else {
                    self.dst.mkdir(&dst_path)?;
                }
                let mut children = self.src.list(src_path)?;
                children.sort_by(|a, b| a.name.cmp(&b.name));
                for c in children {
                    let s = fs::join(src_path, &c.name);
                    let d = fs::join(&dst_path, &c.name);
                    match self.copy_entry(&s, &d, false) {
                        Ok(_) => {}
                        Err(err) if is_cancelled(&err) => return Err(err),
                        Err(err) => self.notes.push(format!("{err:#}")),
                    }
                }
                if let Some(mode) = e.mode {
                    let _ = self.dst.set_mode(&dst_path, mode);
                }
                Ok(Some(fs::file_name(&dst_path)))
            }
            Kind::File => {
                let mut dst_path = dst_path.to_path_buf();
                if self.dst.exists(&dst_path)? {
                    match self.resolve(&dst_path, false)? {
                        Some(p) => dst_path = p,
                        None => {
                            self.done_bytes += e.size;
                            return Ok(None);
                        }
                    }
                }
                self.copy_file(src_path, &dst_path)?;
                if let Some(mode) = e.mode {
                    let _ = self.dst.set_mode(&dst_path, mode);
                }
                if let Some(mtime) = e.modified {
                    let _ = self.dst.set_mtime(&dst_path, mtime);
                }
                Ok(Some(fs::file_name(&dst_path)))
            }
            Kind::Other => {
                self.notes
                    .push(format!("skipped special file {}", src_path.display()));
                Ok(None)
            }
        }
    }

    /// Writes to `<name>.rcp-part` and renames into place, so a failed or
    /// cancelled copy never leaves a truncated file under the real name.
    fn copy_file(&mut self, src_path: &Path, dst_path: &Path) -> Result<()> {
        let part = dst_path.with_file_name(format!("{}{PART_SUFFIX}", fs::file_name(dst_path)));
        let current = fs::file_name(src_path);
        let result = (|| -> Result<()> {
            let mut r = self.src.open_read(src_path)?;
            let mut w = self.dst.open_write(&part)?;
            let mut buf = vec![0u8; CHUNK];
            loop {
                self.check_cancel()?;
                let n = r.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n])?;
                self.done_bytes += n as u64;
                (self.on_progress)(self.done_bytes, &current);
            }
            w.flush()?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = self.dst.remove_file(&part);
            return Err(e);
        }
        if let Err(e) = self.dst.rename(&part, dst_path) {
            // SFTP rename refuses to replace an existing file on many servers.
            if self.dst.exists(dst_path)? {
                self.dst.remove_file(dst_path)?;
                self.dst.rename(&part, dst_path)?;
            } else {
                let _ = self.dst.remove_file(&part);
                return Err(e);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::local::LocalFs;
    use std::fs as stdfs;
    use std::os::unix::fs::PermissionsExt;

    fn run(
        src: &Path,
        dst: &Path,
        answer: Resolution,
        cancel: &AtomicBool,
    ) -> (Result<Option<String>>, Vec<String>) {
        let mut progress = |_: u64, _: &str| {};
        let mut ask = |_: &Path, _: bool| answer;
        let mut c = Copier {
            src: &LocalFs,
            dst: &LocalFs,
            cancel,
            on_progress: &mut progress,
            ask: &mut ask,
            overwrite_all: false,
            notes: vec![],
            done_bytes: 0,
        };
        let r = c.copy_into(src, dst);
        (r, c.notes)
    }

    #[test]
    fn names() {
        assert_eq!(numbered_name("photo.jpg", 1, false), "photo (1).jpg");
        assert_eq!(numbered_name("a.tar.gz", 2, false), "a.tar (2).gz");
        assert_eq!(numbered_name(".bashrc", 1, false), ".bashrc (1)");
        assert_eq!(numbered_name("dir.d", 1, true), "dir.d (1)");
    }

    #[test]
    fn copies_tree_with_modes_and_symlinks() {
        let s = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        let root = s.path().join("proj");
        stdfs::create_dir_all(root.join("a/empty")).unwrap();
        stdfs::write(root.join("a/f.txt"), "hello").unwrap();
        stdfs::set_permissions(root.join("a/f.txt"), stdfs::Permissions::from_mode(0o750)).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("loop")).unwrap();
        std::os::unix::fs::symlink(root.join("a/f.txt"), root.join("link.txt")).unwrap();

        let cancel = AtomicBool::new(false);
        let (r, notes) = run(&root, d.path(), Resolution::Cancel, &cancel);
        assert_eq!(r.unwrap().as_deref(), Some("proj"));
        let out = d.path().join("proj");
        assert_eq!(stdfs::read_to_string(out.join("a/f.txt")).unwrap(), "hello");
        assert_eq!(
            stdfs::metadata(out.join("a/f.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert!(out.join("a/empty").is_dir());
        // File symlinks are copied as content; directory symlinks skipped.
        assert_eq!(
            stdfs::read_to_string(out.join("link.txt")).unwrap(),
            "hello"
        );
        assert!(!out.join("loop").exists());
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn conflict_resolutions() {
        let s = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        stdfs::write(s.path().join("x.txt"), "new").unwrap();
        stdfs::write(d.path().join("x.txt"), "old").unwrap();
        let cancel = AtomicBool::new(false);
        let src = s.path().join("x.txt");

        let (r, _) = run(&src, d.path(), Resolution::Skip, &cancel);
        assert_eq!(r.unwrap(), None);
        assert_eq!(
            stdfs::read_to_string(d.path().join("x.txt")).unwrap(),
            "old"
        );

        let (r, _) = run(&src, d.path(), Resolution::Rename, &cancel);
        assert_eq!(r.unwrap().as_deref(), Some("x (1).txt"));
        assert_eq!(
            stdfs::read_to_string(d.path().join("x (1).txt")).unwrap(),
            "new"
        );

        let (r, _) = run(&src, d.path(), Resolution::Overwrite, &cancel);
        assert_eq!(r.unwrap().as_deref(), Some("x.txt"));
        assert_eq!(
            stdfs::read_to_string(d.path().join("x.txt")).unwrap(),
            "new"
        );

        let (r, _) = run(&src, d.path(), Resolution::Cancel, &cancel);
        assert!(is_cancelled(&r.unwrap_err()));
    }

    #[test]
    fn cancel_leaves_no_partial_file() {
        let s = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        stdfs::write(s.path().join("big"), vec![7u8; CHUNK * 3]).unwrap();
        let cancel = AtomicBool::new(false);
        let mut progress = |_: u64, _: &str| cancel.store(true, Ordering::Relaxed);
        let mut ask = |_: &Path, _: bool| Resolution::Cancel;
        let mut c = Copier {
            src: &LocalFs,
            dst: &LocalFs,
            cancel: &cancel,
            on_progress: &mut progress,
            ask: &mut ask,
            overwrite_all: false,
            notes: vec![],
            done_bytes: 0,
        };
        let r = c.copy_into(&s.path().join("big"), d.path());
        assert!(is_cancelled(&r.unwrap_err()));
        assert_eq!(stdfs::read_dir(d.path()).unwrap().count(), 0);
    }
}
