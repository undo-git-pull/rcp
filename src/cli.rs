use std::path::{Path, PathBuf};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "rcp", version, about = "Two-pane TUI file copier (local and SSH)")]
pub struct Args {
    /// Local directory for the left pane (default: current directory)
    pub left: Option<PathBuf>,
    /// Right pane: a local path, or an scp-style remote like user@host:/path
    pub right: Option<String>,
    /// SSH port (otherwise from ~/.ssh/config, then 22)
    #[arg(short, long)]
    pub port: Option<u16>,
    /// SSH private key
    #[arg(short, long)]
    pub identity: Option<PathBuf>,
    /// Show dotfiles on startup
    #[arg(long)]
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    Local(PathBuf),
    /// `path` is empty for the remote home directory.
    Remote { target: RemoteTarget, path: String },
}

/// Parses `user@host:path`, `host:path`, `host:`, `[::1]:path` or a local
/// path. Anything that exists locally is treated as local, so local files
/// with a `:` in the name still work.
pub fn parse_location(s: &str) -> Location {
    if Path::new(s).exists() {
        return Location::Local(PathBuf::from(s));
    }
    let (user, rest) = match s.split_once('@') {
        Some((u, r)) if !u.is_empty() && !u.contains('/') && !u.contains(':') => (Some(u.to_string()), r),
        _ => (None, s),
    };
    let (host, path) = if let Some(stripped) = rest.strip_prefix('[') {
        match stripped.split_once("]:") {
            Some((h, p)) => (h, p),
            None => return Location::Local(PathBuf::from(s)),
        }
    } else {
        match rest.split_once(':') {
            Some((h, p)) => (h, p),
            None => return Location::Local(PathBuf::from(s)),
        }
    };
    if host.is_empty() || host.contains('/') {
        return Location::Local(PathBuf::from(s));
    }
    Location::Remote {
        target: RemoteTarget { user, host: host.to_string(), port: None },
        path: path.to_string(),
    }
}

pub fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(user: Option<&str>, host: &str, path: &str) -> Location {
        Location::Remote {
            target: RemoteTarget { user: user.map(String::from), host: host.into(), port: None },
            path: path.into(),
        }
    }

    #[test]
    fn parses_remote_forms() {
        assert_eq!(parse_location("nas:/volume1"), remote(None, "nas", "/volume1"));
        assert_eq!(parse_location("ray@10.0.0.5:/srv/www"), remote(Some("ray"), "10.0.0.5", "/srv/www"));
        assert_eq!(parse_location("host:"), remote(None, "host", ""));
        assert_eq!(parse_location("host:rel/dir"), remote(None, "host", "rel/dir"));
        assert_eq!(parse_location("u@[::1]:/tmp"), remote(Some("u"), "::1", "/tmp"));
    }

    #[test]
    fn parses_local_forms() {
        assert_eq!(parse_location("/tmp"), Location::Local("/tmp".into()));
        assert_eq!(parse_location("some/dir"), Location::Local("some/dir".into()));
        assert_eq!(parse_location("./a:b"), Location::Local("./a:b".into()));
        assert_eq!(parse_location("[::1"), Location::Local("[::1".into()));
    }

    #[test]
    fn existing_local_path_with_colon_wins() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a:b");
        std::fs::create_dir(&p).unwrap();
        let s = p.to_str().unwrap();
        assert_eq!(parse_location(s), Location::Local(p.clone()));
    }
}
