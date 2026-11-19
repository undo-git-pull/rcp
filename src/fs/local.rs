use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use anyhow::{Context, Result};
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};

use super::{Backend, CaseMode, Emit, Entry, GrepQuery, Kind, SearchEvent, WalkOpts};

/// Longest line text kept for a grep hit; ranges past this are dropped.
pub const MAX_LINE_LEN: usize = 500;

pub struct LocalFs;

fn entry_from(name: String, path: &Path) -> Result<Entry> {
    let lmeta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let is_symlink = lmeta.file_type().is_symlink();
    // A dangling symlink falls back to the link's own metadata.
    let meta = if is_symlink {
        fs::metadata(path).unwrap_or(lmeta)
    } else {
        lmeta
    };
    let ft = meta.file_type();
    let kind = if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    Ok(Entry {
        name,
        kind,
        is_symlink,
        size: meta.len(),
        modified: meta.modified().ok(),
        mode: Some(meta.permissions().mode()),
    })
}

fn rel(root: &Path, path: &Path) -> String {
    let r = path.strip_prefix(root).unwrap_or(path);
    r.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn walk_builder(root: &Path, hidden: bool, respect_ignore: bool) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.hidden(!hidden)
        .ignore(respect_ignore)
        .git_ignore(respect_ignore)
        .git_global(respect_ignore)
        .git_exclude(respect_ignore)
        .parents(respect_ignore)
        .require_git(false)
        // .git is never interesting, even with hidden files shown.
        .filter_entry(|e| e.file_name() != ".git");
    b
}

/// Truncates a line to `MAX_LINE_LEN` bytes (on a char boundary) and drops
/// match ranges that no longer fit.
pub fn clip_hit(
    line: &str,
    ranges: Vec<std::ops::Range<usize>>,
) -> (String, Vec<std::ops::Range<usize>>) {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.len() <= MAX_LINE_LEN {
        return (line.to_string(), ranges);
    }
    let mut cut = MAX_LINE_LEN;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    let ranges = ranges
        .into_iter()
        .filter(|r| r.start < cut)
        .map(|r| r.start..r.end.min(cut))
        .collect();
    (line[..cut].to_string(), ranges)
}

impl Backend for LocalFs {
    fn label(&self) -> String {
        "Local".into()
    }

    fn is_remote(&self) -> bool {
        false
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))
    }

    fn list(&self, dir: &Path) -> Result<Vec<Entry>> {
        let rd = fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))?;
        let mut out = Vec::new();
        for de in rd {
            let de = de?;
            let name = de.file_name().to_string_lossy().into_owned();
            // Entries can vanish between readdir and stat; skip them.
            if let Ok(e) = entry_from(name, &de.path()) {
                out.push(e);
            }
        }
        Ok(out)
    }

    fn stat(&self, path: &Path) -> Result<Entry> {
        entry_from(super::file_name(path), path)
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
        }
    }

    fn open_read(&self, path: &Path) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(
            File::open(path).with_context(|| format!("open {}", path.display()))?,
        ))
    }

    fn open_write(&self, path: &Path) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(
            File::create(path).with_context(|| format!("create {}", path.display()))?,
        ))
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        fs::create_dir(path).with_context(|| format!("mkdir {}", path.display()))
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
    }

    fn remove_dir(&self, path: &Path) -> Result<()> {
        fs::remove_dir(path).with_context(|| format!("rmdir {}", path.display()))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        fs::rename(from, to)
            .with_context(|| format!("rename {} -> {}", from.display(), to.display()))
    }

    fn set_mode(&self, path: &Path, mode: u32) -> Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
            .with_context(|| format!("chmod {}", path.display()))
    }

    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> Result<()> {
        let f = File::options().write(true).open(path)?;
        f.set_modified(mtime)
            .with_context(|| format!("set mtime {}", path.display()))
    }

    fn walk_files(
        &self,
        root: &Path,
        opts: WalkOpts,
        emit: Emit,
        cancel: &AtomicBool,
    ) -> Result<()> {
        walk_builder(root, opts.hidden, opts.respect_ignore)
            .build_parallel()
            .run(|| {
                Box::new(move |res| {
                    if cancel.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    }
                    if let Ok(e) = res
                        && e.file_type().is_some_and(|t| t.is_file())
                    {
                        emit(SearchEvent::Path(rel(root, e.path())));
                    }
                    WalkState::Continue
                })
            });
        Ok(())
    }

    fn grep(&self, root: &Path, q: &GrepQuery, emit: Emit, cancel: &AtomicBool) -> Result<()> {
        let matcher = RegexMatcherBuilder::new()
            .case_smart(q.case == CaseMode::Smart)
            .case_insensitive(q.case == CaseMode::Insensitive)
            .word(q.whole_word)
            .fixed_strings(!q.regex)
            .build(&q.pattern)?;
        let mut wb = walk_builder(root, q.hidden, q.respect_ignore);
        if !q.globs.is_empty() {
            let mut ob = OverrideBuilder::new(root);
            for g in &q.globs {
                ob.add(g)?;
            }
            wb.overrides(ob.build()?);
        }
        wb.build_parallel().run(|| {
            let matcher = matcher.clone();
            let mut searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .line_number(true)
                .build();
            Box::new(move |res| {
                if cancel.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                let Ok(e) = res else {
                    return WalkState::Continue;
                };
                if !e.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Continue;
                }
                let path = rel(root, e.path());
                let result = searcher.search_path(
                    &matcher,
                    e.path(),
                    Lossy(|line_no, line| {
                        let mut ranges = Vec::new();
                        matcher.find_iter(line.as_bytes(), |m| {
                            ranges.push(m.start()..m.end());
                            true
                        })?;
                        let (line, ranges) = clip_hit(line, ranges);
                        emit(SearchEvent::Match {
                            path: path.clone(),
                            line_no,
                            line,
                            ranges,
                        });
                        Ok(!cancel.load(Ordering::Relaxed))
                    }),
                );
                if let Err(err) = result {
                    emit(SearchEvent::Error(format!("{path}: {err}")));
                }
                WalkState::Continue
            })
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn collect(f: impl FnOnce(Emit)) -> Vec<SearchEvent> {
        let out = Mutex::new(Vec::new());
        f(&|e| out.lock().unwrap().push(e));
        out.into_inner().unwrap()
    }

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        fs::create_dir_all(p.join("src/sub")).unwrap();
        fs::create_dir_all(p.join("target")).unwrap();
        fs::write(
            p.join("src/main.rs"),
            "fn main() {\n    copy_entry(\"x\");\n}\n",
        )
        .unwrap();
        fs::write(p.join("src/sub/lib.rs"), "pub fn Copy_Entry() {}\n").unwrap();
        fs::write(p.join("target/out.rs"), "copy_entry\n").unwrap();
        fs::write(p.join(".hidden.rs"), "copy_entry\n").unwrap();
        fs::write(p.join("bin.dat"), b"copy_entry\0\x01\x02").unwrap();
        fs::write(p.join(".gitignore"), "target/\n").unwrap();
        d
    }

    fn paths(ev: &[SearchEvent]) -> Vec<String> {
        let mut v: Vec<String> = ev
            .iter()
            .filter_map(|e| match e {
                SearchEvent::Path(p) => Some(p.clone()),
                SearchEvent::Match { path, .. } => Some(path.clone()),
                SearchEvent::Error(_) => None,
            })
            .collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn walk_respects_ignore_and_hidden() {
        let d = tree();
        let cancel = AtomicBool::new(false);
        let ev = collect(|emit| {
            LocalFs
                .walk_files(
                    d.path(),
                    WalkOpts {
                        hidden: false,
                        respect_ignore: true,
                    },
                    emit,
                    &cancel,
                )
                .unwrap()
        });
        assert_eq!(paths(&ev), vec!["bin.dat", "src/main.rs", "src/sub/lib.rs"]);

        let ev = collect(|emit| {
            LocalFs
                .walk_files(
                    d.path(),
                    WalkOpts {
                        hidden: true,
                        respect_ignore: false,
                    },
                    emit,
                    &cancel,
                )
                .unwrap()
        });
        assert_eq!(
            paths(&ev),
            vec![
                ".gitignore",
                ".hidden.rs",
                "bin.dat",
                "src/main.rs",
                "src/sub/lib.rs",
                "target/out.rs"
            ]
        );
    }

    #[test]
    fn grep_smart_case_skips_binary_and_reports_ranges() {
        let d = tree();
        let cancel = AtomicBool::new(false);
        let q = GrepQuery {
            pattern: "copy_entry".into(),
            regex: true,
            respect_ignore: true,
            ..Default::default()
        };
        let ev = collect(|emit| LocalFs.grep(d.path(), &q, emit, &cancel).unwrap());
        // Smart case: lowercase pattern matches Copy_Entry too; binary file skipped.
        assert_eq!(paths(&ev), vec!["src/main.rs", "src/sub/lib.rs"]);
        let hit = ev
            .iter()
            .find_map(|e| match e {
                SearchEvent::Match {
                    path,
                    line_no,
                    line,
                    ranges,
                } if path == "src/main.rs" => Some((*line_no, line.clone(), ranges.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(hit.0, 2);
        assert_eq!(&hit.1[hit.2[0].clone()], "copy_entry");

        let q = GrepQuery {
            case: CaseMode::Sensitive,
            ..q
        };
        let ev = collect(|emit| LocalFs.grep(d.path(), &q, emit, &cancel).unwrap());
        assert_eq!(paths(&ev), vec!["src/main.rs"]);
    }

    #[test]
    fn grep_fixed_string_and_globs() {
        let d = tree();
        let cancel = AtomicBool::new(false);
        let q = GrepQuery {
            pattern: "main(".into(),
            regex: false,
            respect_ignore: true,
            ..Default::default()
        };
        let ev = collect(|emit| LocalFs.grep(d.path(), &q, emit, &cancel).unwrap());
        assert_eq!(paths(&ev), vec!["src/main.rs"]);

        let q = GrepQuery {
            pattern: "copy".into(),
            globs: vec!["!src/sub/**".into()],
            ..q
        };
        let ev = collect(|emit| LocalFs.grep(d.path(), &q, emit, &cancel).unwrap());
        assert_eq!(paths(&ev), vec!["src/main.rs"]);
    }

    #[test]
    fn clip_hit_keeps_char_boundaries() {
        let long = "é".repeat(400); // 800 bytes
        let (l, r) = clip_hit(&long, vec![0..2, 600..602]);
        assert!(l.len() <= MAX_LINE_LEN);
        assert_eq!(r, vec![0..2]);
    }
}
