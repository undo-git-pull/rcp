//! Search modals: fuzzy file finder and content search. Both search the
//! active pane's cwd recursively on that pane's backend.

pub mod finder;
pub mod grep;
pub mod input;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::event::{AppEvent, PreviewTarget, Side};
use crate::fs::{Backend, CaseMode, GrepQuery, SearchEvent, WalkOpts};
use crate::preview::{self, Preview};
use finder::Finder;
use grep::{Hit, MAX_HITS, Results};
use input::Input;

const GREP_DEBOUNCE: Duration = Duration::from_millis(250);
const PREVIEW_DELAY: Duration = Duration::from_millis(50);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Ids for searches and preview requests, unique across modal instances so
/// late events from a closed modal never match a new one.
pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub enum Action {
    None,
    Close,
    /// Put the pane on this result (path relative to the search root).
    Jump(String),
    /// Copy these results (relative paths) to the other pane.
    Copy(Vec<String>),
}

pub struct GrepState {
    pub results: Results,
    pub regex: bool,
    pub case: CaseMode,
    pub whole_word: bool,
    pub glob: Input,
    pub glob_focus: bool,
    pub truncated: bool,
    pending: Option<Instant>,
}

pub enum Mode {
    Finder(Finder),
    Grep(GrepState),
}

type SelKey = (String, u64);

pub struct SearchModal {
    pub side: Side,
    pub backend: Arc<dyn Backend>,
    pub root: PathBuf,
    pub mode: Mode,
    pub input: Input,
    pub hidden: bool,
    pub respect_ignore: bool,
    pub cursor: usize,
    pub offset: usize,
    pub marked: BTreeSet<String>,
    pub running: bool,
    pub error: Option<String>,
    pub preview: Option<Preview>,
    /// Set by the renderer so preview scrolling knows the page size.
    pub preview_height: usize,
    pub search_id: u64,
    pub preview_id: u64,
    cancel: Arc<AtomicBool>,
    tx: Sender<AppEvent>,
    loaded_key: Option<SelKey>,
    pending_key: Option<(SelKey, Instant)>,
}

impl SearchModal {
    fn new(side: Side, backend: Arc<dyn Backend>, root: PathBuf, mode: Mode, hidden: bool, tx: Sender<AppEvent>) -> Self {
        SearchModal {
            side,
            backend,
            root,
            mode,
            input: Input::default(),
            hidden,
            respect_ignore: true,
            cursor: 0,
            offset: 0,
            marked: BTreeSet::new(),
            running: false,
            error: None,
            preview: None,
            preview_height: 10,
            search_id: 0,
            preview_id: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            tx,
            loaded_key: None,
            pending_key: None,
        }
    }

    pub fn finder(side: Side, backend: Arc<dyn Backend>, root: PathBuf, hidden: bool, tx: Sender<AppEvent>) -> Self {
        let mut m = Self::new(side, backend, root, Mode::Finder(Finder::new()), hidden, tx);
        m.start();
        m
    }

    pub fn grep(side: Side, backend: Arc<dyn Backend>, root: PathBuf, hidden: bool, tx: Sender<AppEvent>) -> Self {
        let state = GrepState {
            results: Results::default(),
            regex: true,
            case: CaseMode::Smart,
            whole_word: false,
            glob: Input::default(),
            glob_focus: false,
            truncated: false,
            pending: None,
        };
        Self::new(side, backend, root, Mode::Grep(state), hidden, tx)
    }

    pub fn is_finder(&self) -> bool {
        matches!(self.mode, Mode::Finder(_))
    }

    pub fn result_count(&self) -> usize {
        match &self.mode {
            Mode::Finder(f) => f.matched() as usize,
            Mode::Grep(g) => g.results.count,
        }
    }

    /// Relative path (and grep hit) under the cursor.
    pub fn selected(&self) -> Option<(String, Option<Hit>)> {
        match &self.mode {
            Mode::Finder(f) => f.get(self.cursor as u32).map(|p| (p, None)),
            Mode::Grep(g) => g.results.get(self.cursor).map(|(p, h)| (p.to_string(), Some(h.clone()))),
        }
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        rel.split('/').fold(self.root.clone(), |p, c| p.join(c))
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// (Re)starts the walk or grep for the current query and options.
    fn start(&mut self) {
        self.cancel();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.search_id = next_id();
        self.cursor = 0;
        self.offset = 0;
        self.error = None;
        let (id, cancel, tx, backend, root) =
            (self.search_id, self.cancel.clone(), self.tx.clone(), self.backend.clone(), self.root.clone());
        match &mut self.mode {
            Mode::Finder(f) => {
                let injector = f.restart();
                let opts = WalkOpts { hidden: self.hidden, respect_ignore: self.respect_ignore };
                self.running = true;
                thread::spawn(move || {
                    let count = AtomicU64::new(0);
                    let emit_tx = tx.clone();
                    let emit = move |ev: SearchEvent| match ev {
                        SearchEvent::Path(p) => {
                            if count.fetch_add(1, Ordering::Relaxed) < finder::MAX_PATHS as u64 {
                                finder::push(&injector, p);
                            }
                        }
                        SearchEvent::Error(e) => {
                            let _ = emit_tx.send(AppEvent::Search { id, event: SearchEvent::Error(e) });
                        }
                        SearchEvent::Match { .. } => {}
                    };
                    let r = backend.walk_files(&root, opts, &emit, &cancel);
                    let _ = tx.send(AppEvent::SearchDone { id, error: r.err().map(|e| format!("{e:#}")) });
                });
            }
            Mode::Grep(g) => {
                g.results.clear();
                g.truncated = false;
                g.pending = None;
                if self.input.text.is_empty() {
                    self.running = false;
                    return;
                }
                let q = GrepQuery {
                    pattern: self.input.text.clone(),
                    regex: g.regex,
                    case: g.case,
                    whole_word: g.whole_word,
                    hidden: self.hidden,
                    respect_ignore: self.respect_ignore,
                    globs: g.glob.text.split_whitespace().map(String::from).collect(),
                };
                self.running = true;
                thread::spawn(move || {
                    let emit_tx = tx.clone();
                    let emit = move |event: SearchEvent| {
                        let _ = emit_tx.send(AppEvent::Search { id, event });
                    };
                    let r = backend.grep(&root, &q, &emit, &cancel);
                    let _ = tx.send(AppEvent::SearchDone { id, error: r.err().map(|e| format!("{e:#}")) });
                });
            }
        }
    }

    pub fn on_search_event(&mut self, event: SearchEvent) {
        match (event, &mut self.mode) {
            (SearchEvent::Match { path, line_no, line, ranges }, Mode::Grep(g)) => {
                if g.results.count >= MAX_HITS {
                    g.truncated = true;
                    self.cancel.store(true, Ordering::Relaxed);
                    return;
                }
                g.results.push(path, Hit { line_no, line, ranges });
            }
            (SearchEvent::Error(e), _) => self.error = Some(e),
            _ => {}
        }
    }

    pub fn on_done(&mut self, error: Option<String>) {
        self.running = false;
        if error.is_some() {
            self.error = error;
        }
    }

    pub fn on_preview(&mut self, id: u64, preview: Preview) {
        if id == self.preview_id {
            self.preview = Some(preview);
        }
    }

    /// Called every frame: advances the matcher, fires a debounced grep and
    /// loads the preview once the selection has settled.
    pub fn tick(&mut self) {
        match &mut self.mode {
            Mode::Finder(f) => f.tick(),
            Mode::Grep(g) => {
                if g.pending.is_some_and(|t| t.elapsed() >= GREP_DEBOUNCE) {
                    self.start();
                }
            }
        }
        let n = self.result_count();
        if self.cursor >= n {
            self.cursor = n.saturating_sub(1);
        }

        let Some((rel, hit)) = self.selected() else {
            self.preview = None;
            self.loaded_key = None;
            return;
        };
        let key = (rel.clone(), hit.as_ref().map_or(0, |h| h.line_no));
        if self.loaded_key.as_ref() == Some(&key) {
            return;
        }
        match &self.pending_key {
            Some((k, t)) if *k == key => {
                if t.elapsed() < PREVIEW_DELAY {
                    return;
                }
            }
            _ => {
                self.pending_key = Some((key, Instant::now()));
                return;
            }
        }
        self.loaded_key = Some(key);
        self.pending_key = None;
        self.preview_id = next_id();
        let (id, tx, backend, path) = (self.preview_id, self.tx.clone(), self.backend.clone(), self.abs(&rel));
        thread::spawn(move || {
            let preview = match hit {
                Some(h) => preview::load_around(&*backend, &path, h.line_no, h.ranges),
                None => preview::load(&*backend, &path),
            };
            let _ = tx.send(AppEvent::Preview { target: PreviewTarget::Search, id, preview });
        });
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = self.result_count();
        if n == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).clamp(0, n as isize - 1) as usize;
    }

    fn query_changed(&mut self) {
        self.cursor = 0;
        self.offset = 0;
        match &mut self.mode {
            Mode::Finder(f) => f.set_query(&self.input.text),
            Mode::Grep(g) => g.pending = Some(Instant::now()),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = self.preview_height.max(2) as isize - 1;
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('c') if ctrl => return Action::Close,
            KeyCode::Enter => {
                if let Some((rel, _)) = self.selected() {
                    return Action::Jump(rel);
                }
            }
            KeyCode::Char('y') if ctrl => {
                let paths: Vec<String> = if !self.marked.is_empty() {
                    self.marked.iter().cloned().collect()
                } else {
                    self.selected().map(|(p, _)| p).into_iter().collect()
                };
                if !paths.is_empty() {
                    return Action::Copy(paths);
                }
            }
            KeyCode::Tab => {
                if let Some((rel, _)) = self.selected()
                    && !self.marked.remove(&rel)
                {
                    self.marked.insert(rel);
                }
                self.move_cursor(1);
            }
            KeyCode::Up | KeyCode::Down if shift => {
                let d = if key.code == KeyCode::Up { -1 } else { 1 };
                let h = self.preview_height;
                if let Some(p) = &mut self.preview {
                    p.scroll_by(d, h);
                }
            }
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k' | 'p') if ctrl => self.move_cursor(-1),
            KeyCode::Char('j' | 'n') if ctrl => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::PageDown => self.move_cursor(page),
            KeyCode::Char('h') if alt => {
                self.hidden = !self.hidden;
                self.start();
            }
            KeyCode::Char('i') if alt => {
                self.respect_ignore = !self.respect_ignore;
                self.start();
            }
            _ => {
                if let Mode::Grep(g) = &mut self.mode {
                    let restart = match key.code {
                        KeyCode::Char('r') if alt => {
                            g.regex = !g.regex;
                            true
                        }
                        KeyCode::Char('c') if alt => {
                            g.case = match g.case {
                                CaseMode::Smart => CaseMode::Sensitive,
                                CaseMode::Sensitive => CaseMode::Insensitive,
                                CaseMode::Insensitive => CaseMode::Smart,
                            };
                            true
                        }
                        KeyCode::Char('w') if alt => {
                            g.whole_word = !g.whole_word;
                            true
                        }
                        KeyCode::Char('g') if alt => {
                            g.glob_focus = !g.glob_focus;
                            false
                        }
                        _ if g.glob_focus => {
                            if g.glob.handle(key) {
                                g.pending = Some(Instant::now());
                            }
                            false
                        }
                        _ => {
                            if self.input.handle(key) {
                                self.query_changed();
                            }
                            false
                        }
                    };
                    if restart {
                        self.start();
                    }
                } else if self.input.handle(key) {
                    self.query_changed();
                }
            }
        }
        Action::None
    }

    pub fn root_display(&self) -> String {
        format!("{}:{}", self.backend.label(), Path::new(&self.root).display())
    }
}

impl Drop for SearchModal {
    fn drop(&mut self) {
        self.cancel();
    }
}
