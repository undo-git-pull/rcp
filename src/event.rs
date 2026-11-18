//! Messages sent from background threads to the UI thread.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use anyhow::Result;

use crate::fs::{Entry, SearchEvent};
use crate::ops::copy::Resolution;
use crate::preview::Preview;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }

    pub fn idx(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewTarget {
    Main,
    Search,
}

pub enum JobEvent {
    Progress {
        done_bytes: u64,
        total_bytes: u64,
        current: String,
    },
    /// The worker blocks until a `Resolution` is sent back.
    Conflict {
        path: String,
        is_dir: bool,
        reply: Sender<Resolution>,
    },
    Finished {
        message: String,
        is_error: bool,
        refresh: Vec<Side>,
        /// Put the cursor on this name in that pane after refreshing.
        select: Option<(Side, String)>,
    },
}

pub enum AppEvent {
    Job(JobEvent),
    Listing {
        side: Side,
        id: u64,
        result: Result<(PathBuf, Vec<Entry>)>,
    },
    Preview {
        target: PreviewTarget,
        id: u64,
        preview: Preview,
    },
    DeleteSummary {
        id: u64,
        summary: String,
    },
    Search {
        id: u64,
        event: SearchEvent,
    },
    SearchDone {
        id: u64,
        error: Option<String>,
    },
}
