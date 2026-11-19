//! ProxyJump through the OpenSSH client.
//!
//! libssh2 only runs over a real socket, so a jumped session gets one end of
//! a socketpair whose other end is the stdin/stdout of `ssh -W host:port`.
//! One control master to the last jump host is started at connect time, when
//! ssh may still prompt; every tunnel after that is multiplexed over it, so
//! exec sessions and reconnects never authenticate to the jump hosts again.

#[cfg(unix)]
pub use imp::Jump;

#[cfg(not(unix))]
pub use stub::Jump;

/// Splits a `-J`/`ProxyJump` list into hops. `None` for an empty list or
/// `none`, which OpenSSH uses to switch jumping off.
pub fn hops(spec: &str) -> Option<Vec<String>> {
    let hops: Vec<String> = spec
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if hops.is_empty() || (hops.len() == 1 && hops[0].eq_ignore_ascii_case("none")) {
        None
    } else {
        Some(hops)
    }
}

/// A `-J` hop (`[user@]host[:port]`) as a destination ssh accepts on its
/// own command line, where `host:port` isn't understood.
fn dest_uri(hop: &str) -> String {
    if hop.starts_with("ssh://") {
        hop.to_string()
    } else {
        format!("ssh://{hop}")
    }
}

/// `host:port` for `ssh -W`, with IPv6 addresses bracketed.
fn forward_spec(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::{self, DirBuilder};
    use std::net::Shutdown;
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use anyhow::{Context, Result, bail};

    use super::{dest_uri, forward_spec};

    pub struct Jump {
        /// `-J` list for the hops before the last one.
        via: Option<String>,
        /// The last hop, which the master connects to.
        last: String,
        /// Private directory holding the control socket.
        dir: PathBuf,
        sock: PathBuf,
        master: Mutex<Option<Child>>,
    }

    impl Jump {
        /// Starts the control master. Runs before the TUI, so ssh may
        /// prompt on the terminal.
        pub fn start(hops: &[String]) -> Result<Jump> {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let Some((last, via)) = hops.split_last() else {
                bail!("empty jump host list")
            };
            // Socket paths are limited to ~100 bytes (104 on macOS) and ssh
            // appends 17 characters while binding, so a long $TMPDIR won't do.
            let tmp = std::env::temp_dir();
            let tmp = if tmp.as_os_str().len() > 60 {
                PathBuf::from("/tmp")
            } else {
                tmp
            };
            let dir = tmp.join(format!(
                "rcp-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .with_context(|| format!("create {}", dir.display()))?;
            let j = Jump {
                via: (!via.is_empty()).then(|| via.join(",")),
                last: dest_uri(last),
                sock: dir.join("ctl"),
                dir,
                master: Mutex::new(None),
            };
            let master = j.spawn_master(true)?;
            *j.master.lock().unwrap() = Some(master);
            Ok(j)
        }

        fn ssh(&self) -> Command {
            let mut cmd = Command::new("ssh");
            cmd.arg("-S")
                .arg(&self.sock)
                .args(["-o", "ConnectTimeout=10", "-o", "LogLevel=ERROR"]);
            if let Some(via) = &self.via {
                cmd.arg("-J").arg(via);
            }
            cmd
        }

        /// Runs `ssh -M -N` and waits until its control socket appears,
        /// which happens once it has authenticated.
        fn spawn_master(&self, interactive: bool) -> Result<Child> {
            // A master that was killed leaves its socket behind, and a new
            // one would then run without multiplexing.
            let _ = fs::remove_file(&self.sock);
            let mut cmd = self.ssh();
            cmd.args(["-M", "-N", "-o", "ControlPersist=no"]);
            if !interactive {
                cmd.args(["-o", "BatchMode=yes"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null());
            }
            let mut child = cmd
                .arg(&self.last)
                .spawn()
                .context("run ssh (ProxyJump needs the OpenSSH client on PATH)")?;
            loop {
                if self.sock.exists() {
                    return Ok(child);
                }
                if let Some(st) = child.try_wait()? {
                    bail!("ssh to jump host {} failed ({st})", self.last);
                }
                thread::sleep(Duration::from_millis(50));
            }
        }

        /// A stream to `host:port` as seen from the last jump host. If the
        /// master has died (say the laptop slept), it is restarted without
        /// prompting, which works with agent or unencrypted keys.
        pub fn tunnel(&self, host: &str, port: u16, show_errors: bool) -> Result<Tunnel> {
            {
                let mut master = self.master.lock().unwrap();
                let alive = matches!(master.as_mut().map(|c| c.try_wait()), Some(Ok(None)));
                if !alive {
                    if let Some(mut c) = master.take() {
                        let _ = c.wait();
                    }
                    *master = Some(self.spawn_master(false)?);
                }
            }
            let (ours, theirs) = UnixStream::pair()?;
            let out = theirs.try_clone()?;
            let child = self
                .ssh()
                .args(["-o", "ControlMaster=no", "-o", "BatchMode=yes", "-W"])
                .arg(forward_spec(host, port))
                .arg(&self.last)
                .stdin(OwnedFd::from(theirs))
                .stdout(OwnedFd::from(out))
                .stderr(if show_errors {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .spawn()
                .context("run ssh -W")?;
            Ok(Tunnel { sock: ours, child })
        }
    }

    impl Drop for Jump {
        fn drop(&mut self) {
            if let Some(mut c) = self.master.get_mut().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Our end of an `ssh -W` tunnel; the ssh process goes with it.
    pub struct Tunnel {
        sock: UnixStream,
        child: Child,
    }

    impl AsRawFd for Tunnel {
        fn as_raw_fd(&self) -> RawFd {
            self.sock.as_raw_fd()
        }
    }

    impl Drop for Tunnel {
        fn drop(&mut self) {
            let _ = self.sock.shutdown(Shutdown::Both);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(not(unix))]
mod stub {
    use anyhow::{Result, bail};

    pub struct Jump;
    pub type Tunnel = std::net::TcpStream;

    impl Jump {
        pub fn start(_hops: &[String]) -> Result<Jump> {
            bail!("ProxyJump is only supported on Unix")
        }

        pub fn tunnel(&self, _host: &str, _port: u16, _show_errors: bool) -> Result<Tunnel> {
            unreachable!("Jump can't be constructed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_hops() {
        assert_eq!(
            hops("a, b@c:2222,"),
            Some(vec!["a".into(), "b@c:2222".into()])
        );
        assert_eq!(hops("none"), None);
        assert_eq!(hops("NONE"), None);
        assert_eq!(hops(" "), None);
    }

    #[test]
    fn hop_destinations() {
        assert_eq!(dest_uri("bastion"), "ssh://bastion");
        assert_eq!(dest_uri("ray@10.0.0.1:2222"), "ssh://ray@10.0.0.1:2222");
        assert_eq!(dest_uri("ssh://[::1]:22"), "ssh://[::1]:22");
        assert_eq!(forward_spec("db.internal", 22), "db.internal:22");
        assert_eq!(forward_spec("fe80::1", 2222), "[fe80::1]:2222");
    }
}
