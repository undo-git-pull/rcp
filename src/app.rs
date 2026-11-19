//! App state and key handling. Slow work goes to background threads; their
//! results come back as `AppEvent`s.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use humansize::{BINARY, format_size};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::event::{AppEvent, JobEvent, PreviewTarget, Side};
use crate::fs::{self, Backend};
use crate::ops::copy::Resolution;
use crate::ops::delete;
use crate::ops::worker::{self, Job};
use crate::pane::Pane;
use crate::preview::{self, Preview};
use crate::search::{self, Action, SearchModal};
use crate::ui;

pub enum Modal {
    ConfirmDelete {
        side: Side,
        paths: Vec<PathBuf>,
        what: String,
        summary: Option<String>,
        id: u64,
    },
    ConfirmCopy {
        side: Side,
        paths: Vec<PathBuf>,
        what: String,
        dest: String,
    },
    Conflict {
        path: String,
        is_dir: bool,
        reply: Sender<Resolution>,
    },
    ConfirmQuit,
    ConfirmCancel,
    Help,
}

pub struct JobStatus {
    pub done: u64,
    pub total: u64,
    pub current: String,
    pub started: Instant,
    pub is_delete: bool,
}

impl JobStatus {
    fn new(is_delete: bool) -> JobStatus {
        JobStatus {
            done: 0,
            total: 0,
            current: String::new(),
            started: Instant::now(),
            is_delete,
        }
    }
}

pub struct App {
    pub panes: [Pane; 2],
    pub active: Side,
    /// Preview of the active pane's selection, shown in place of the other pane.
    pub preview: Option<Preview>,
    preview_id: u64,
    preview_path: Option<PathBuf>,
    pub preview_height: usize,
    pub modal: Option<Modal>,
    pub search: Option<SearchModal>,
    pub status: String,
    pub status_error: bool,
    pub job: Option<JobStatus>,
    /// `is_delete` for each job waiting behind the running one.
    pub queued: VecDeque<bool>,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    jobs: Sender<Job>,
    cancel: Arc<AtomicBool>,
    quit: bool,
}

impl App {
    pub fn new(left: Pane, right: Pane) -> App {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let jobs = worker::spawn(tx.clone(), cancel.clone());
        let mut app = App {
            panes: [left, right],
            active: Side::Left,
            preview: None,
            preview_id: 0,
            preview_path: None,
            preview_height: 10,
            modal: None,
            search: None,
            status: "? for help".into(),
            status_error: false,
            job: None,
            queued: VecDeque::new(),
            tx,
            rx,
            jobs,
            cancel,
            quit: false,
        };
        app.reload(Side::Left);
        app.reload(Side::Right);
        app
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            if let Some(s) = &mut self.search {
                s.tick();
            }
            terminal.draw(|f| ui::draw(f, &mut self))?;
            if event::poll(Duration::from_millis(50))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                self.on_key(k);
            }
            while let Ok(ev) = self.rx.try_recv() {
                self.on_event(ev);
            }
        }
        Ok(())
    }

    pub fn pane(&self, side: Side) -> &Pane {
        &self.panes[side.idx()]
    }

    fn pane_mut(&mut self, side: Side) -> &mut Pane {
        &mut self.panes[side.idx()]
    }

    fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        self.status = msg.into();
        self.status_error = is_error;
    }

    /// Lists the pane's cwd: inline for local, on a thread for remote.
    fn reload(&mut self, side: Side) {
        let p = self.pane_mut(side);
        p.list_id += 1;
        let (id, backend, cwd) = (p.list_id, p.backend.clone(), p.cwd.clone());
        let list = move |b: &dyn Backend| -> Result<(PathBuf, Vec<fs::Entry>)> {
            let cwd = b.canonicalize(&cwd)?;
            let entries = b.list(&cwd)?;
            Ok((cwd, entries))
        };
        if backend.is_remote() {
            p.loading = true;
            let tx = self.tx.clone();
            thread::spawn(move || {
                let result = list(&*backend);
                let _ = tx.send(AppEvent::Listing { side, id, result });
            });
        } else {
            let result = list(&*backend);
            self.on_listing(side, id, result);
        }
    }

    fn on_listing(&mut self, side: Side, id: u64, result: Result<(PathBuf, Vec<fs::Entry>)>) {
        let p = self.pane_mut(side);
        if id != p.list_id {
            return;
        }
        match result {
            Ok((cwd, entries)) => p.set_entries(cwd, entries),
            Err(e) => {
                p.loading = false;
                p.error = Some(format!("{e:#}"));
            }
        }
        if side == self.active {
            self.refresh_preview();
        }
    }

    fn cd(&mut self, side: Side, dir: PathBuf, select: Option<String>) {
        let p = self.pane_mut(side);
        p.cwd = dir;
        p.select_after_load = select;
        p.cursor = 0;
        self.reload(side);
    }

    fn enter_dir(&mut self) {
        let p = self.pane(self.active);
        let Some(e) = p.selected() else { return };
        if e.name == ".." {
            self.go_up();
        } else if e.is_dir() {
            let dir = fs::join(&p.cwd, &e.name);
            self.cd(self.active, dir, None);
        }
    }

    fn go_up(&mut self) {
        let p = self.pane(self.active);
        if let Some(parent) = fs::parent(&p.cwd) {
            let name = fs::file_name(&p.cwd);
            self.cd(self.active, parent, Some(name));
        }
    }

    fn refresh_preview(&mut self) {
        if self.preview.is_none() {
            return;
        }
        let p = self.pane(self.active);
        let (path, backend) = (p.selected_path(), p.backend.clone());
        if path == self.preview_path {
            return;
        }
        self.preview_path = path.clone();
        self.preview_id = search::next_id();
        let Some(path) = path else {
            self.preview = Some(Preview::message("..", "parent directory"));
            return;
        };
        let (id, tx) = (self.preview_id, self.tx.clone());
        thread::spawn(move || {
            let preview = preview::load(&*backend, &path);
            let _ = tx.send(AppEvent::Preview {
                target: PreviewTarget::Main,
                id,
                preview,
            });
        });
    }

    fn toggle_preview(&mut self) {
        if self.preview.take().is_none() {
            self.preview = Some(Preview::message("", "loading…"));
            self.preview_path = None;
            self.refresh_preview();
        }
    }

    fn start_copy(&mut self, src_side: Side, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let src = self.pane(src_side);
        let dst = self.pane(src_side.other());
        let dst_dir = dst.cwd.clone();
        // Two local panes are the same filesystem; guard against copying
        // a file onto itself or a directory into itself.
        if src.backend.label() == dst.backend.label() {
            for p in &paths {
                if p.parent() == Some(dst_dir.as_path()) {
                    return self.set_status("Source and destination are the same", true);
                }
                if dst_dir.starts_with(p) {
                    return self.set_status("Can't copy a directory into itself", true);
                }
            }
        }
        // Directory copies can be large, so they ask first.
        let dirs: Vec<&str> = paths
            .iter()
            .filter(|p| p.parent() == Some(src.cwd.as_path()))
            .filter_map(|p| {
                src.items
                    .iter()
                    .find(|e| e.name == fs::file_name(p) && e.is_dir())
            })
            .map(|e| e.name.as_str())
            .collect();
        if !dirs.is_empty() {
            let what = match (paths.len(), dirs.len()) {
                (1, _) => format!("directory \"{}/\"", dirs[0]),
                (n, 1) => format!("{n} items (1 directory)"),
                (n, d) => format!("{n} items ({d} directories)"),
            };
            let dest = format!("{}:{}", dst.backend.label(), dst_dir.display());
            self.modal = Some(Modal::ConfirmCopy {
                side: src_side,
                paths,
                what,
                dest,
            });
            return;
        }
        self.submit_copy(src_side, paths);
    }

    fn submit_copy(&mut self, src_side: Side, paths: Vec<PathBuf>) {
        let (src, dst) = (self.pane(src_side), self.pane(src_side.other()));
        let dst_dir = dst.cwd.clone();
        let job = Job::Copy {
            src: src.backend.clone(),
            dst: dst.backend.clone(),
            paths,
            dst_dir,
            dst_side: src_side.other(),
        };
        self.pane_mut(src_side).marked.clear();
        self.submit(job, false);
    }

    fn submit(&mut self, job: Job, is_delete: bool) {
        if self.job.is_some() {
            self.queued.push_back(is_delete);
        } else {
            self.job = Some(JobStatus::new(is_delete));
        }
        let _ = self.jobs.send(job);
    }

    fn ask_delete(&mut self) {
        let side = self.active;
        let p = self.pane(side);
        let paths = p.targets();
        if paths.is_empty() {
            return;
        }
        let what = if paths.len() == 1 {
            let e = p.selected().filter(|_| p.marked.is_empty());
            let is_dir = e.is_some_and(|e| e.is_dir() && !e.is_symlink);
            format!(
                "{} \"{}\"",
                if is_dir { "directory" } else { "file" },
                fs::file_name(&paths[0])
            )
        } else {
            format!("{} marked items", paths.len())
        };
        let id = search::next_id();
        let (tx, backend, ps) = (self.tx.clone(), p.backend.clone(), paths.clone());
        thread::spawn(move || {
            let never = AtomicBool::new(false);
            let mut total = delete::Summary::default();
            for p in &ps {
                match delete::summarize(&*backend, p, &never) {
                    Ok(s) => {
                        total.files += s.files;
                        total.dirs += s.dirs;
                        total.bytes += s.bytes;
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::DeleteSummary {
                            id,
                            summary: format!("{e:#}"),
                        });
                        return;
                    }
                }
            }
            let size = format_size(total.bytes, BINARY);
            let summary = match (total.files, total.dirs) {
                (1, 0) => size,
                (f, 0) => format!("{f} files, {size}"),
                (f, d) => format!("{f} files in {d} folders, {size}"),
            };
            let _ = tx.send(AppEvent::DeleteSummary { id, summary });
        });
        self.modal = Some(Modal::ConfirmDelete {
            side,
            paths,
            what,
            summary: None,
            id,
        });
    }

    fn on_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Listing { side, id, result } => self.on_listing(side, id, result),
            AppEvent::Preview {
                target: PreviewTarget::Main,
                id,
                preview,
            } => {
                if id == self.preview_id && self.preview.is_some() {
                    self.preview = Some(preview);
                }
            }
            AppEvent::Preview {
                target: PreviewTarget::Search,
                id,
                preview,
            } => {
                if let Some(s) = &mut self.search {
                    s.on_preview(id, preview);
                }
            }
            AppEvent::DeleteSummary {
                id: got,
                summary: s,
            } => {
                if let Some(Modal::ConfirmDelete { id, summary, .. }) = &mut self.modal
                    && *id == got
                {
                    *summary = Some(s);
                }
            }
            AppEvent::Search { id, event } => {
                if let Some(s) = &mut self.search
                    && s.search_id == id
                {
                    s.on_search_event(event);
                }
            }
            AppEvent::SearchDone { id, error } => {
                if let Some(s) = &mut self.search
                    && s.search_id == id
                {
                    s.on_done(error);
                }
            }
            AppEvent::Job(JobEvent::Progress {
                done_bytes,
                total_bytes,
                current,
            }) => {
                if let Some(j) = &mut self.job {
                    j.done = done_bytes;
                    j.total = total_bytes;
                    j.current = current;
                }
            }
            AppEvent::Job(JobEvent::Conflict {
                path,
                is_dir,
                reply,
            }) => {
                self.modal = Some(Modal::Conflict {
                    path,
                    is_dir,
                    reply,
                });
            }
            AppEvent::Job(JobEvent::Finished {
                message,
                is_error,
                refresh,
                select,
            }) => {
                if matches!(self.modal, Some(Modal::ConfirmCancel)) {
                    self.modal = None;
                }
                self.job = self.queued.pop_front().map(JobStatus::new);
                self.set_status(message, is_error);
                for side in refresh {
                    if let Some((s, name)) = &select
                        && *s == side
                    {
                        self.pane_mut(side).select_after_load = Some(name.clone());
                    }
                    self.reload(side);
                }
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if self.modal.is_some() {
            return self.on_modal_key(key);
        }
        if self.search.is_some() {
            return self.on_search_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.preview_height.max(2) as isize - 1;
        let side = self.active;
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('c') if ctrl => self.request_quit(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.active = side.other();
                self.preview_path = None;
                self.refresh_preview();
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::PageDown => self.move_cursor(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_cursor(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_cursor(isize::MAX / 2),
            KeyCode::Right | KeyCode::Char('l') => self.enter_dir(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => self.go_up(),
            KeyCode::Enter => {
                if self.pane(side).selected().is_some_and(|e| e.is_dir()) {
                    self.enter_dir();
                } else {
                    self.toggle_preview();
                }
            }
            KeyCode::Char(' ') => {
                let paths = self.pane(side).targets();
                self.start_copy(side, paths);
            }
            KeyCode::Char('d') | KeyCode::Delete => self.ask_delete(),
            KeyCode::Char('v') => {
                self.pane_mut(side).toggle_mark();
                self.refresh_preview();
            }
            KeyCode::Char('.') => {
                let p = self.pane_mut(side);
                p.show_hidden = !p.show_hidden;
                p.rebuild();
            }
            KeyCode::Char('s') => {
                let p = self.pane_mut(side);
                p.sort = p.sort.next();
                p.rebuild();
                let label = p.sort.label();
                self.set_status(format!("Sort by {label}"), false);
            }
            KeyCode::Char('r') => {
                for s in [Side::Left, Side::Right] {
                    let b = self.pane(s).backend.clone();
                    if b.is_remote()
                        && self.pane(s).error.is_some()
                        && let Err(e) = b.reconnect()
                    {
                        self.set_status(format!("Reconnect failed: {e:#}"), true);
                    }
                    self.reload(s);
                }
            }
            KeyCode::Char('/') => self.open_search(true),
            KeyCode::Char('p') if ctrl => self.open_search(true),
            KeyCode::Char('f') if ctrl => self.open_search(false),
            KeyCode::Char('?') | KeyCode::F(1) => self.modal = Some(Modal::Help),
            KeyCode::Esc => {
                if self.preview.is_some() {
                    self.preview = None;
                } else if self.job.is_some() {
                    self.modal = Some(Modal::ConfirmCancel);
                } else {
                    self.pane_mut(side).marked.clear();
                }
            }
            _ => {}
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let side = self.active;
        self.pane_mut(side).move_cursor(delta);
        self.refresh_preview();
    }

    fn request_quit(&mut self) {
        if self.job.is_some() {
            self.modal = Some(Modal::ConfirmQuit);
        } else {
            self.quit = true;
        }
    }

    fn open_search(&mut self, finder: bool) {
        let p = self.pane(self.active);
        let (side, backend, root, hidden, tx) = (
            self.active,
            p.backend.clone(),
            p.cwd.clone(),
            p.show_hidden,
            self.tx.clone(),
        );
        self.search = Some(if finder {
            SearchModal::finder(side, backend, root, hidden, tx)
        } else {
            SearchModal::grep(side, backend, root, hidden, tx)
        });
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        let Some(s) = &mut self.search else { return };
        match s.on_key(key) {
            Action::None => {}
            Action::Close => self.search = None,
            Action::Jump(rel) => {
                let s = self.search.take().unwrap();
                let abs = s.abs(&rel);
                let dir = abs
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or(s.root.clone());
                self.active = s.side;
                self.cd(s.side, dir, Some(fs::file_name(&abs)));
            }
            Action::Copy(rels) => {
                let paths: Vec<PathBuf> = rels.iter().map(|r| s.abs(r)).collect();
                let side = s.side;
                s.marked.clear();
                self.start_copy(side, paths);
            }
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        let c = match key.code {
            KeyCode::Char(c) => c.to_ascii_lowercase(),
            KeyCode::Esc => '\x1b',
            KeyCode::Enter => '\n',
            _ => '\0',
        };
        match modal {
            Modal::ConfirmDelete { side, paths, .. } if c == 'y' => {
                self.pane_mut(side).marked.clear();
                self.submit(
                    Job::Delete {
                        backend: self.pane(side).backend.clone(),
                        paths,
                        side,
                    },
                    true,
                );
            }
            Modal::ConfirmDelete { .. } => self.set_status("Delete cancelled", false),
            Modal::ConfirmCopy { side, paths, .. } if c == 'y' => self.submit_copy(side, paths),
            Modal::ConfirmCopy { .. } => self.set_status("Copy cancelled", false),
            Modal::Conflict {
                path,
                is_dir,
                reply,
            } => {
                let r = match c {
                    'o' => Resolution::Overwrite,
                    's' => Resolution::Skip,
                    'r' => Resolution::Rename,
                    'a' => Resolution::OverwriteAll,
                    'c' | '\x1b' => Resolution::Cancel,
                    _ => {
                        self.modal = Some(Modal::Conflict {
                            path,
                            is_dir,
                            reply,
                        });
                        return;
                    }
                };
                let _ = reply.send(r);
            }
            Modal::ConfirmQuit if c == 'y' => {
                self.cancel.store(true, Ordering::Relaxed);
                self.quit = true;
            }
            Modal::ConfirmCancel if c == 'y' => {
                self.cancel.store(true, Ordering::Relaxed);
                self.set_status("Cancelling…", false);
            }
            Modal::ConfirmQuit | Modal::ConfirmCancel | Modal::Help => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::local::LocalFs;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app(left: &Path, right: &Path) -> App {
        let l = Pane::new(Arc::new(LocalFs), left.to_path_buf(), false);
        let r = Pane::new(Arc::new(LocalFs), right.to_path_buf(), false);
        App::new(l, r)
    }

    fn screen(app: &mut App) -> String {
        let mut t = Terminal::new(TestBackend::new(100, 24)).unwrap();
        t.draw(|f| ui::draw(f, app)).unwrap();
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Feeds background events until `done` holds or a timeout passes.
    fn pump(app: &mut App, done: impl Fn(&App) -> bool) {
        let start = Instant::now();
        while !done(app) && start.elapsed() < Duration::from_secs(5) {
            if let Some(s) = &mut app.search {
                s.tick();
            }
            if let Ok(ev) = app.rx.recv_timeout(Duration::from_millis(20)) {
                app.on_event(ev);
            }
        }
        assert!(done(app), "timed out");
    }

    #[test]
    fn main_view_preview_and_delete_modal() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        std::fs::write(l.path().join("hello.txt"), "first line\nsecond\n").unwrap();
        std::fs::create_dir(l.path().join("sub")).unwrap();
        let mut a = app(l.path(), r.path());

        let s = screen(&mut a);
        assert!(s.contains("Local: "));
        assert!(s.contains("sub/"));
        assert!(s.contains("hello.txt"));

        // Enter opens directories (and ".." goes back up) instead of previewing.
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Enter);
        assert!(a.pane(Side::Left).cwd.ends_with("sub"));
        assert!(a.preview.is_none());
        key(&mut a, KeyCode::Enter);
        assert_eq!(a.pane(Side::Left).selected().unwrap().name, "sub");
        key(&mut a, KeyCode::Up);

        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Enter);
        pump(&mut a, |a| {
            a.preview.as_ref().is_some_and(|p| p.title == "hello.txt")
        });
        let s = screen(&mut a);
        assert!(s.contains("Preview: hello.txt"));
        assert!(s.contains("first line"));

        key(&mut a, KeyCode::Char('d'));
        pump(&mut a, |a| {
            matches!(
                &a.modal,
                Some(Modal::ConfirmDelete {
                    summary: Some(_),
                    ..
                })
            )
        });
        let s = screen(&mut a);
        assert!(s.contains("Delete file \"hello.txt\" from Local?"));
        key(&mut a, KeyCode::Char('n'));
        assert!(a.modal.is_none());
        assert!(l.path().join("hello.txt").exists());
    }

    #[test]
    fn copy_then_conflict_modal() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        std::fs::write(l.path().join("a.txt"), "x").unwrap();
        let mut a = app(l.path(), r.path());
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Char(' '));
        pump(&mut a, |a| a.job.is_none());
        assert!(r.path().join("a.txt").exists());
        assert!(a.status.starts_with("Copied 1 item"));

        key(&mut a, KeyCode::Char(' '));
        pump(&mut a, |a| matches!(a.modal, Some(Modal::Conflict { .. })));
        assert!(screen(&mut a).contains("This file already exists."));
        key(&mut a, KeyCode::Char('s'));
        pump(&mut a, |a| a.job.is_none());
        assert_eq!(std::fs::read_dir(r.path()).unwrap().count(), 1);
    }

    #[test]
    fn directory_copy_asks_first() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        std::fs::create_dir(l.path().join("d")).unwrap();
        std::fs::write(l.path().join("d/f"), "x").unwrap();
        let mut a = app(l.path(), r.path());
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Char(' '));
        assert!(screen(&mut a).contains("Copy directory \"d/\" to Local:"));
        key(&mut a, KeyCode::Char('n'));
        assert!(a.job.is_none());
        assert!(!r.path().join("d").exists());

        key(&mut a, KeyCode::Char(' '));
        key(&mut a, KeyCode::Char('y'));
        pump(&mut a, |a| a.job.is_none());
        assert!(r.path().join("d/f").exists());
    }

    #[test]
    fn search_modals_render_results() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        std::fs::create_dir(l.path().join("src")).unwrap();
        std::fs::write(
            l.path().join("src/main.rs"),
            "fn main() {\n    needle();\n}\n",
        )
        .unwrap();
        let mut a = app(l.path(), r.path());

        key(&mut a, KeyCode::Char('/'));
        for c in "smain".chars() {
            key(&mut a, KeyCode::Char(c));
        }
        pump(&mut a, |a| {
            a.search
                .as_ref()
                .is_some_and(|s| s.result_count() == 1 && s.preview.is_some())
        });
        let s = screen(&mut a);
        assert!(s.contains("Find files in"));
        assert!(s.contains("src/main.rs"));
        key(&mut a, KeyCode::Esc);

        a.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        for c in "needle".chars() {
            key(&mut a, KeyCode::Char(c));
        }
        pump(&mut a, |a| {
            a.search
                .as_ref()
                .is_some_and(|s| s.result_count() == 1 && s.preview.is_some())
        });
        let s = screen(&mut a);
        assert!(s.contains("1 hits"));
        assert!(s.contains("2: needle();"));

        key(&mut a, KeyCode::Enter);
        assert!(a.search.is_none());
        assert!(a.pane(Side::Left).cwd.ends_with("src"));
        assert_eq!(a.pane(Side::Left).selected().unwrap().name, "main.rs");
    }
}
