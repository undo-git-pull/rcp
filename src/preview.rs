//! Loading file previews: text, hex dump or directory summary.

use std::io::{BufRead, BufReader, Read};
use std::ops::Range;
use std::path::Path;

use anyhow::Result;
use humansize::{BINARY, format_size};

use crate::fs::{self, Backend, Kind};

const HEAD_BYTES: u64 = 64 * 1024;
const HEX_BYTES: usize = 4096;
const DIR_NAMES: usize = 200;
/// Lines kept on each side of a grep hit.
const CONTEXT: u64 = 200;
/// Stop reading toward a grep hit after this many bytes.
const MAX_SCAN: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum Body {
    /// `first_line` is the 1-based number of `lines[0]`.
    Text {
        first_line: u64,
        lines: Vec<String>,
        numbered: bool,
    },
    Hex(Vec<String>),
    Dir(Vec<String>),
    Message(String),
}

#[derive(Clone, Debug)]
pub struct Preview {
    pub title: String,
    pub info: String,
    pub body: Body,
    /// Line (1-based) to highlight and centre, with match byte ranges.
    pub focus: Option<(u64, Vec<Range<usize>>)>,
    /// None means "centre on focus" (or top when there is no focus).
    pub scroll: Option<usize>,
    pub truncated: bool,
}

impl Preview {
    pub fn message(title: &str, msg: impl Into<String>) -> Preview {
        Preview {
            title: title.into(),
            info: String::new(),
            body: Body::Message(msg.into()),
            focus: None,
            scroll: None,
            truncated: false,
        }
    }

    pub fn line_count(&self) -> usize {
        match &self.body {
            Body::Text { lines, .. } | Body::Hex(lines) | Body::Dir(lines) => lines.len(),
            Body::Message(_) => 1,
        }
    }

    pub fn scroll_by(&mut self, delta: isize, height: usize) {
        let cur = self.scroll.unwrap_or_else(|| self.auto_scroll(height));
        let max = self.line_count().saturating_sub(1);
        self.scroll = Some((cur as isize + delta).clamp(0, max as isize) as usize);
    }

    /// Scroll offset that centres the focus line in a view of `height`.
    pub fn auto_scroll(&self, height: usize) -> usize {
        match (&self.body, &self.focus) {
            (Body::Text { first_line, .. }, Some((line, _))) => {
                let idx = line.saturating_sub(*first_line) as usize;
                idx.saturating_sub(height / 2)
            }
            _ => 0,
        }
    }
}

pub fn expand_tabs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0;
    for c in s.chars() {
        if c == '\t' {
            let n = 4 - col % 4;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else if c.is_control() {
            out.push('·');
            col += 1;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

/// Valid UTF-8 (allowing a char cut off at the end) without NUL bytes.
fn as_text(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s),
        Err(e) if e.error_len().is_none() => {
            Some(std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap())
        }
        Err(_) => None,
    }
}

pub fn hex_dump(bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(16)
        .enumerate()
        .map(|(i, chunk)| {
            let mut hex = String::new();
            for (j, b) in chunk.iter().enumerate() {
                if j == 8 {
                    hex.push(' ');
                }
                hex.push_str(&format!("{b:02x} "));
            }
            let ascii: String = chunk
                .iter()
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{:08x}  {hex:<49} |{ascii}|", i * 16)
        })
        .collect()
}

fn info_line(b: &dyn Backend, path: &Path) -> Result<(String, crate::fs::Entry)> {
    let e = b.stat(path)?;
    let mut parts = vec![];
    if e.kind != Kind::Dir {
        parts.push(format_size(e.size, BINARY));
    }
    if let Some(m) = e.modified {
        parts.push(
            chrono::DateTime::<chrono::Local>::from(m)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
        );
    }
    if let Some(mode) = e.mode {
        parts.push(format_mode(mode, e.kind == Kind::Dir));
    }
    if e.is_symlink {
        parts.push("symlink".into());
    }
    Ok((parts.join("  "), e))
}

pub fn format_mode(mode: u32, is_dir: bool) -> String {
    let mut s = String::from(if is_dir { "d" } else { "-" });
    for shift in [6, 3, 0] {
        let m = (mode >> shift) & 7;
        s.push(if m & 4 != 0 { 'r' } else { '-' });
        s.push(if m & 2 != 0 { 'w' } else { '-' });
        s.push(if m & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Preview of a file (first 64 KiB) or directory.
pub fn load(b: &dyn Backend, path: &Path) -> Preview {
    let title = fs::file_name(path);
    match try_load(b, path, &title) {
        Ok(p) => p,
        Err(e) => Preview::message(&title, format!("{e:#}")),
    }
}

fn try_load(b: &dyn Backend, path: &Path, title: &str) -> Result<Preview> {
    let (info, e) = info_line(b, path)?;
    let mut p = Preview::message(title, "");
    p.info = info;
    match e.kind {
        Kind::Dir => {
            let mut entries = b.list(path)?;
            entries.sort_by(|a, b| {
                b.is_dir()
                    .cmp(&a.is_dir())
                    .then_with(|| a.name.cmp(&b.name))
            });
            let n = entries.len();
            let mut lines = vec![format!("{n} entries"), String::new()];
            lines.extend(entries.iter().take(DIR_NAMES).map(|e| {
                if e.is_dir() {
                    format!("{}/", e.name)
                } else {
                    e.name.clone()
                }
            }));
            p.truncated = n > DIR_NAMES;
            p.body = Body::Dir(lines);
        }
        Kind::File => {
            let mut buf = Vec::new();
            b.open_read(path)?.take(HEAD_BYTES).read_to_end(&mut buf)?;
            p.truncated = e.size > buf.len() as u64;
            p.body = match as_text(&buf) {
                Some("") => Body::Message("(empty file)".into()),
                Some(s) => Body::Text {
                    first_line: 1,
                    lines: s.lines().map(expand_tabs).collect(),
                    numbered: false,
                },
                None => {
                    p.truncated = e.size > HEX_BYTES as u64;
                    Body::Hex(hex_dump(&buf[..buf.len().min(HEX_BYTES)]))
                }
            };
        }
        Kind::Other => p.body = Body::Message("(special file)".into()),
    }
    Ok(p)
}

/// Preview of the lines around `line_no`, for grep results.
pub fn load_around(
    b: &dyn Backend,
    path: &Path,
    line_no: u64,
    ranges: Vec<Range<usize>>,
) -> Preview {
    let title = fs::file_name(path);
    let result = (|| -> Result<Preview> {
        let (info, _) = info_line(b, path)?;
        let first = line_no.saturating_sub(CONTEXT).max(1);
        let last = line_no + CONTEXT;
        let reader = BufReader::new(b.open_read(path)?.take(MAX_SCAN));
        let mut lines = vec![];
        for (i, line) in reader.split(b'\n').enumerate() {
            let n = i as u64 + 1;
            if n > last {
                break;
            }
            let line = line?;
            if n >= first {
                let s = String::from_utf8_lossy(&line);
                lines.push(expand_tabs(s.trim_end_matches('\r')));
            }
        }
        let mut p = Preview::message(&title, "");
        p.info = info;
        p.body = if lines.is_empty() {
            Body::Message(format!(
                "line {line_no} is past the first {}",
                format_size(MAX_SCAN, BINARY)
            ))
        } else {
            Body::Text {
                first_line: first,
                lines,
                numbered: true,
            }
        };
        p.focus = Some((line_no, ranges));
        Ok(p)
    })();
    result.unwrap_or_else(|e| Preview::message(&title, format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::local::LocalFs;

    #[test]
    fn hex_format() {
        let lines = hex_dump(b"ABCDEFGHIJKLMNOPQ\x00");
        assert_eq!(
            lines[0],
            "00000000  41 42 43 44 45 46 47 48  49 4a 4b 4c 4d 4e 4f 50  |ABCDEFGHIJKLMNOP|"
        );
        assert!(lines[1].starts_with("00000010  51 00 "));
        assert!(lines[1].ends_with("|Q.|"));
    }

    #[test]
    fn text_detection() {
        assert_eq!(as_text(b"hi\n"), Some("hi\n"));
        assert_eq!(as_text(b"x\x00"), None);
        // A multi-byte char cut at the 64 KiB boundary is still text.
        assert_eq!(as_text(&"é".as_bytes()[..1]), Some(""));
        assert_eq!(as_text(b"\xff\xfe"), None);
        assert_eq!(expand_tabs("a\tb"), "a   b");
    }

    #[test]
    fn around_hit() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("f.txt");
        let text: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&f, text).unwrap();
        let p = load_around(&LocalFs, &f, 500, vec![0..4]);
        match &p.body {
            Body::Text {
                first_line, lines, ..
            } => {
                assert_eq!(*first_line, 300);
                assert_eq!(lines[200], "line 500");
                assert_eq!(lines.len(), 401);
            }
            b => panic!("{b:?}"),
        }
        assert_eq!(p.auto_scroll(20), 190);
    }
}
