mod app;
mod cli;
mod event;
mod fs;
mod ops;
mod pane;
mod preview;
mod search;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use cli::{Args, Location, expand_tilde, parse_location};
use fs::Backend;
use fs::local::LocalFs;
use fs::sftp::{ConnectOpts, SftpFs};
use pane::Pane;

fn open(loc: Location, args: &Args) -> Result<(Arc<dyn Backend>, PathBuf)> {
    match loc {
        Location::Local(p) => {
            let p = expand_tilde(&p);
            let p = std::fs::canonicalize(&p).with_context(|| format!("{}", p.display()))?;
            Ok((Arc::new(LocalFs), p))
        }
        Location::Remote { mut target, path } => {
            target.port = args.port.or(target.port);
            let opts = ConnectOpts { identity: args.identity.clone(), jump: args.jump.clone() };
            let b = SftpFs::connect(&target, &opts)?;
            let cwd = b.canonicalize(&PathBuf::from(&path))?;
            Ok((Arc::new(b), cwd))
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let left = Location::Local(args.left.clone().unwrap_or_else(|| PathBuf::from(".")));
    let (lb, lcwd) = open(left, &args)?;
    let right = match &args.right {
        Some(s) => parse_location(s),
        None => Location::Local(lcwd.clone()),
    };
    // Connecting may prompt for host keys or passwords, so do it before
    // the terminal switches to raw mode.
    let (rb, rcwd) = open(right, &args)?;

    let app = app::App::new(Pane::new(lb, lcwd, args.hidden), Pane::new(rb, rcwd, args.hidden));
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}
