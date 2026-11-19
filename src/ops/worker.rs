//! Background thread that runs copy and delete jobs one at a time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use humansize::{BINARY, format_size};

use super::copy::{self, Copier, Resolution, is_cancelled};
use super::delete;
use crate::event::{AppEvent, JobEvent, Side};
use crate::fs::Backend;

const PROGRESS_EVERY: Duration = Duration::from_millis(100);

pub enum Job {
    Copy {
        src: Arc<dyn Backend>,
        dst: Arc<dyn Backend>,
        paths: Vec<PathBuf>,
        dst_dir: PathBuf,
        dst_side: Side,
    },
    Delete {
        backend: Arc<dyn Backend>,
        paths: Vec<PathBuf>,
        side: Side,
    },
}

pub fn spawn(tx: Sender<AppEvent>, cancel: Arc<AtomicBool>) -> Sender<Job> {
    let (job_tx, job_rx) = channel::<Job>();
    thread::spawn(move || {
        for job in job_rx {
            cancel.store(false, Ordering::Relaxed);
            let ev = match job {
                Job::Copy {
                    src,
                    dst,
                    paths,
                    dst_dir,
                    dst_side,
                } => run_copy(&tx, &cancel, &*src, &*dst, &paths, &dst_dir, dst_side),
                Job::Delete {
                    backend,
                    paths,
                    side,
                } => run_delete(&tx, &cancel, &*backend, &paths, side),
            };
            if tx.send(AppEvent::Job(ev)).is_err() {
                break;
            }
        }
    });
    job_tx
}

fn progress(tx: &Sender<AppEvent>, done_bytes: u64, total_bytes: u64, current: &str) {
    let _ = tx.send(AppEvent::Job(JobEvent::Progress {
        done_bytes,
        total_bytes,
        current: current.into(),
    }));
}

fn summarize_notes(mut msg: String, notes: &[String]) -> (String, bool) {
    if let Some(first) = notes.first() {
        msg.push_str(&format!(" · {} problem(s): {first}", notes.len()));
    }
    (msg, !notes.is_empty())
}

fn run_copy(
    tx: &Sender<AppEvent>,
    cancel: &AtomicBool,
    src: &dyn Backend,
    dst: &dyn Backend,
    paths: &[PathBuf],
    dst_dir: &Path,
    dst_side: Side,
) -> JobEvent {
    progress(tx, 0, 0, "scanning…");
    let mut total = 0;
    for p in paths {
        match copy::scan(src, p, cancel) {
            Ok((bytes, _)) => total += bytes,
            Err(e) if is_cancelled(&e) => {
                return finished("Copy cancelled".into(), false, dst_side, None);
            }
            Err(_) => {}
        }
    }

    let mut last = Instant::now() - PROGRESS_EVERY;
    let mut on_progress = |done: u64, current: &str| {
        if last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            progress(tx, done, total, current);
        }
    };
    let mut ask = |path: &Path, is_dir: bool| {
        let (reply, rx) = channel();
        let ev = JobEvent::Conflict {
            path: path.display().to_string(),
            is_dir,
            reply,
        };
        if tx.send(AppEvent::Job(ev)).is_err() {
            return Resolution::Cancel;
        }
        rx.recv().unwrap_or(Resolution::Cancel)
    };
    let mut c = Copier {
        src,
        dst,
        cancel,
        on_progress: &mut on_progress,
        ask: &mut ask,
        overwrite_all: false,
        notes: vec![],
        done_bytes: 0,
    };

    let mut copied = 0;
    let mut select = None;
    for p in paths {
        match c.copy_into(p, dst_dir) {
            Ok(Some(name)) => {
                copied += 1;
                select = Some((dst_side, name));
            }
            Ok(None) => {}
            Err(e) if is_cancelled(&e) => {
                let (msg, err) =
                    summarize_notes(format!("Copy cancelled after {copied} item(s)"), &c.notes);
                return finished(msg, err, dst_side, select);
            }
            Err(e) => c.notes.push(format!("{e:#}")),
        }
    }
    let msg = format!(
        "Copied {copied} item(s), {} to {}:{}",
        format_size(c.done_bytes, BINARY),
        dst.label(),
        dst_dir.display()
    );
    let (msg, err) = summarize_notes(msg, &c.notes);
    finished(msg, err, dst_side, select)
}

fn run_delete(
    tx: &Sender<AppEvent>,
    cancel: &AtomicBool,
    b: &dyn Backend,
    paths: &[PathBuf],
    side: Side,
) -> JobEvent {
    let mut removed = 0u64;
    let mut last = Instant::now() - PROGRESS_EVERY;
    let mut notes = vec![];
    for p in paths {
        let mut on_item = |name: &str| {
            removed += 1;
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                progress(tx, removed, 0, name);
            }
        };
        match delete::delete(b, p, cancel, &mut on_item) {
            Ok(()) => {}
            Err(e) if is_cancelled(&e) => {
                return finished(
                    format!("Delete cancelled after {removed} item(s)"),
                    false,
                    side,
                    None,
                );
            }
            Err(e) => notes.push(format!("{e:#}")),
        }
    }
    let (msg, err) = summarize_notes(
        format!("Deleted {removed} item(s) from {}", b.label()),
        &notes,
    );
    finished(msg, err, side, None)
}

fn finished(
    message: String,
    is_error: bool,
    side: Side,
    select: Option<(Side, String)>,
) -> JobEvent {
    JobEvent::Finished {
        message,
        is_error,
        refresh: vec![side],
        select,
    }
}
