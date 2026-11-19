//! Remote backend over SSH/SFTP (libssh2 via the `ssh2` crate).
//!
//! Browsing and transfers share one SFTP session. `ssh2` locks the session
//! per call, so the UI, the copy worker and search threads interleave at
//! call granularity (a 256 KiB read or one readdir at a time). Searches run
//! `fd`/`find`/`rg`/`grep` over exec channels on separate pooled sessions,
//! so a slow remote command never holds the SFTP session's lock.

use std::borrow::Cow;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use ssh2::{
    CheckResult, ErrorCode, ExtendedData, HashType, KeyboardInteractivePrompt, KnownHostFileKind,
    MethodType, OpenFlags, OpenType, Prompt, RenameFlags, Session, Sftp,
};

use super::jump::{self, Jump};
use super::local::clip_hit;
use super::{Backend, CaseMode, Emit, Entry, GrepQuery, Kind, SearchEvent, WalkOpts};
use crate::cli::RemoteTarget;

const IO_BUF: usize = 256 * 1024;
/// Blocking-call timeout on the SFTP session, so a dead link errors out
/// instead of hanging every thread waiting on the session lock.
const SFTP_TIMEOUT_MS: u32 = 60_000;
/// Read timeout on exec channels; bounds how long a cancel takes to notice.
const POLL_MS: u32 = 200;
/// Exit code our scripts use when `cd` into the search root fails.
const NO_ROOT: i32 = 99;
const FALLBACK_DEPTH: usize = 12;
const FX_NO_SUCH_FILE: i32 = 2;
const FX_NO_SUCH_PATH: i32 = 10;

#[derive(Clone, Debug, Default)]
pub struct ConnectOpts {
    pub identity: Option<PathBuf>,
    /// `-J` hops; overrides `ProxyJump` from `~/.ssh/config`.
    pub jump: Option<String>,
}

/// Where to connect, after applying `~/.ssh/config`.
#[derive(Clone, Debug)]
struct Dest {
    alias: String,
    host: String,
    port: u16,
    user: String,
    cfg_keys: Vec<PathBuf>,
    jump: Option<Vec<String>>,
}

/// Credentials that worked, kept so `reconnect` never has to prompt.
#[derive(Clone)]
enum Cred {
    None,
    Agent,
    Key {
        path: PathBuf,
        passphrase: Option<String>,
    },
    Password(String),
    Keyboard(String),
}

struct Conn {
    _sess: Session,
    sftp: Sftp,
}

/// Remote tools found by `detect_tools`, cached per `SftpFs`.
#[derive(Clone, Debug, Default)]
struct Tools {
    /// `sh` runs over exec at all.
    sh: bool,
    /// Binary name (`fd` or `fdfind`) and its version.
    fd: Option<(String, (u32, u32))>,
    rg: Option<(u32, u32)>,
    grep: bool,
    grep_pcre: bool,
}

pub struct SftpFs {
    dest: Dest,
    cred: Cred,
    host_key: Vec<u8>,
    home: PathBuf,
    conn: RwLock<Arc<Conn>>,
    /// Idle sessions for exec channels.
    exec_pool: Mutex<Vec<Session>>,
    tools: Mutex<Option<Tools>>,
    /// Declared last so sessions through the jump close before it does.
    jump: Option<Jump>,
}

impl SftpFs {
    /// Connects and authenticates. Runs before the TUI starts, so it may
    /// prompt on the terminal (host key confirmation, passwords).
    pub fn connect(target: &RemoteTarget, opts: &ConnectOpts) -> Result<SftpFs> {
        let dest = resolve(target, opts);
        let jump = dest.jump.as_deref().map(Jump::start).transpose()?;
        let (sess, host_key) = open_session(
            &dest,
            jump.as_ref(),
            HostCheck::KnownHosts(&known_hosts_path()),
        )?;
        let cred = authenticate(&sess, &dest, opts)?;
        let conn = sftp_conn(sess).with_context(|| format!("sftp {}", dest.alias))?;
        let home = conn
            .sftp
            .realpath(Path::new("."))
            .with_context(|| format!("resolve home on {}", dest.alias))?;
        Ok(SftpFs {
            dest,
            cred,
            host_key,
            home,
            conn: RwLock::new(Arc::new(conn)),
            exec_pool: Mutex::new(Vec::new()),
            tools: Mutex::new(None),
            jump,
        })
    }

    fn sftp(&self) -> Arc<Conn> {
        self.conn.read().unwrap().clone()
    }

    /// A fresh authenticated session, without prompting.
    fn new_session(&self) -> Result<Session> {
        let (sess, _) = open_session(
            &self.dest,
            self.jump.as_ref(),
            HostCheck::Pinned(&self.host_key),
        )?;
        reauth(&sess, &self.dest.user, &self.cred)?;
        Ok(sess)
    }

    /// Empty or relative paths resolve against the remote home.
    fn abs(&self, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.home.join(p)
        }
    }

    fn ctx(&self, op: &str, p: &Path) -> String {
        format!("{op} {}:{}", self.dest.alias, p.display())
    }

    fn entry(&self, sftp: &Sftp, name: String, path: &Path, lstat: ssh2::FileStat) -> Entry {
        let is_symlink = lstat.file_type().is_symlink();
        // A dangling symlink falls back to the link's own metadata.
        let st = if is_symlink {
            sftp.stat(path).unwrap_or(lstat)
        } else {
            lstat
        };
        entry_from(name, is_symlink, &st)
    }

    /// Runs `script` under `sh -c` on a pooled exec session, feeding stdout
    /// to `sink`. Returns the exit status, or `None` if cancelled.
    fn exec(
        &self,
        script: &str,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<Option<i32>> {
        let pooled = self.exec_pool.lock().unwrap().pop();
        let (sess, res) = match pooled {
            // A pooled session may have been dropped by the server while idle.
            Some(s) => match run_exec(&s, script, cancel, sink) {
                Err(e) if e.downcast_ref::<ExecStartError>().is_some() => {
                    let s = self.new_session()?;
                    let r = run_exec(&s, script, cancel, sink);
                    (s, r)
                }
                r => (s, r),
            },
            None => {
                let s = self.new_session()?;
                let r = run_exec(&s, script, cancel, sink);
                (s, r)
            }
        };
        if matches!(res, Ok(Some(_))) {
            let mut pool = self.exec_pool.lock().unwrap();
            if pool.len() < 2 {
                pool.push(sess);
            }
        }
        res.map_err(|e| e.context(format!("exec on {}", self.dest.alias)))
    }

    fn tools(&self) -> Tools {
        let mut cached = self.tools.lock().unwrap();
        if let Some(t) = &*cached {
            return t.clone();
        }
        let mut out = Vec::new();
        let never = AtomicBool::new(false);
        let t = match self.exec(DETECT_SCRIPT, &never, &mut |b| out.extend_from_slice(b)) {
            Ok(_) => parse_tools(&String::from_utf8_lossy(&out)),
            Err(_) => Tools::default(),
        };
        *cached = Some(t.clone());
        t
    }

    /// Streams NUL-separated relative paths from a remote command.
    fn exec_paths(&self, script: &str, root: &Path, emit: Emit, cancel: &AtomicBool) -> Result<()> {
        let mut buf = Vec::new();
        let status = self.exec(script, cancel, &mut |b| {
            buf.extend_from_slice(b);
            drain_records(&mut buf, b'\0', |rec| {
                if !rec.is_empty() {
                    emit(SearchEvent::Path(strip_dot(&String::from_utf8_lossy(rec))));
                }
            });
        })?;
        check_status(status, &self.ctx("walk", root))
    }

    /// Slow fallback when the server allows SFTP only.
    fn walk_sftp(
        &self,
        root: &Path,
        opts: WalkOpts,
        emit: Emit,
        cancel: &AtomicBool,
    ) -> Result<()> {
        emit(SearchEvent::Error(format!(
            "{}: no shell access, walking over SFTP (slow, max depth {FALLBACK_DEPTH})",
            self.dest.alias
        )));
        let conn = self.sftp();
        let mut stack = vec![(root.to_path_buf(), String::new(), 0)];
        while let Some((dir, rel, depth)) = stack.pop() {
            let Ok(items) = conn.sftp.readdir(&dir) else {
                continue;
            };
            for (path, st) in items {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let name = super::file_name(&path);
                if name == ".git" || (!opts.hidden && name.starts_with('.')) {
                    continue;
                }
                let r = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                // lstat data: symlinks are neither, so they are never followed.
                if st.is_dir() && depth < FALLBACK_DEPTH {
                    stack.push((path, r, depth + 1));
                } else if st.is_file() {
                    emit(SearchEvent::Path(r));
                }
            }
        }
        Ok(())
    }
}

impl Backend for SftpFs {
    fn label(&self) -> String {
        format!("{}@{}", self.dest.user, self.dest.alias)
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        let p = self.abs(path);
        self.sftp()
            .sftp
            .realpath(&p)
            .with_context(|| self.ctx("resolve", &p))
    }

    fn list(&self, dir: &Path) -> Result<Vec<Entry>> {
        let dir = self.abs(dir);
        let conn = self.sftp();
        let items = conn
            .sftp
            .readdir(&dir)
            .with_context(|| self.ctx("list", &dir))?;
        Ok(items
            .into_iter()
            .map(|(path, st)| self.entry(&conn.sftp, super::file_name(&path), &path, st))
            .collect())
    }

    fn stat(&self, path: &Path) -> Result<Entry> {
        let p = self.abs(path);
        let conn = self.sftp();
        let lst = conn.sftp.lstat(&p).with_context(|| self.ctx("stat", &p))?;
        Ok(self.entry(&conn.sftp, super::file_name(&p), &p, lst))
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        let p = self.abs(path);
        match self.sftp().sftp.lstat(&p) {
            Ok(_) => Ok(true),
            Err(e) if matches!(e.code(), ErrorCode::SFTP(FX_NO_SUCH_FILE | FX_NO_SUCH_PATH)) => {
                Ok(false)
            }
            Err(e) => Err(e).with_context(|| self.ctx("stat", &p)),
        }
    }

    fn open_read(&self, path: &Path) -> Result<Box<dyn Read + Send>> {
        let p = self.abs(path);
        let f = self
            .sftp()
            .sftp
            .open(&p)
            .with_context(|| self.ctx("open", &p))?;
        Ok(Box::new(BufReader::with_capacity(IO_BUF, f)))
    }

    fn open_write(&self, path: &Path) -> Result<Box<dyn Write + Send>> {
        let p = self.abs(path);
        let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
        let f = self
            .sftp()
            .sftp
            .open_mode(&p, flags, 0o644, OpenType::File)
            .with_context(|| self.ctx("create", &p))?;
        Ok(Box::new(BufWriter::with_capacity(IO_BUF, f)))
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        let p = self.abs(path);
        self.sftp()
            .sftp
            .mkdir(&p, 0o755)
            .with_context(|| self.ctx("mkdir", &p))
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        let p = self.abs(path);
        self.sftp()
            .sftp
            .unlink(&p)
            .with_context(|| self.ctx("remove", &p))
    }

    fn remove_dir(&self, path: &Path) -> Result<()> {
        let p = self.abs(path);
        self.sftp()
            .sftp
            .rmdir(&p)
            .with_context(|| self.ctx("rmdir", &p))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let (f, t) = (self.abs(from), self.abs(to));
        // The flags only reach SFTPv5+ servers; OpenSSH (v3) ignores them and
        // fails if `to` exists, which the caller handles.
        let flags = RenameFlags::OVERWRITE | RenameFlags::ATOMIC | RenameFlags::NATIVE;
        self.sftp()
            .sftp
            .rename(&f, &t, Some(flags))
            .with_context(|| {
                format!(
                    "rename {}:{} -> {}",
                    self.dest.alias,
                    f.display(),
                    t.display()
                )
            })
    }

    fn set_mode(&self, path: &Path, mode: u32) -> Result<()> {
        let p = self.abs(path);
        let st = ssh2::FileStat {
            perm: Some(mode & 0o7777),
            ..blank_stat()
        };
        self.sftp()
            .sftp
            .setstat(&p, st)
            .with_context(|| self.ctx("chmod", &p))
    }

    fn set_mtime(&self, path: &Path, mtime: SystemTime) -> Result<()> {
        let p = self.abs(path);
        let conn = self.sftp();
        let secs = mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // SFTP sets atime and mtime together; keep the current atime.
        let atime = conn
            .sftp
            .stat(&p)
            .ok()
            .and_then(|s| s.atime)
            .unwrap_or(secs);
        let st = ssh2::FileStat {
            atime: Some(atime),
            mtime: Some(secs),
            ..blank_stat()
        };
        conn.sftp
            .setstat(&p, st)
            .with_context(|| self.ctx("set mtime", &p))
    }

    fn walk_files(
        &self,
        root: &Path,
        opts: WalkOpts,
        emit: Emit,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let root = self.abs(root);
        let tools = self.tools();
        let root_s = root.to_string_lossy();
        if let Some((fd, ver)) = &tools.fd {
            self.exec_paths(&fd_script(fd, *ver, &root_s, opts), &root, emit, cancel)
        } else if tools.sh {
            self.exec_paths(&find_script(&root_s, opts), &root, emit, cancel)
        } else {
            self.walk_sftp(&root, opts, emit, cancel)
        }
    }

    fn grep(&self, root: &Path, q: &GrepQuery, emit: Emit, cancel: &AtomicBool) -> Result<()> {
        let root = self.abs(root);
        // Same matcher settings as LocalFs: validates the pattern up front and
        // computes match ranges for `grep`, which doesn't report them.
        let matcher = build_matcher(q)?;
        let tools = self.tools();
        let root_s = root.to_string_lossy();
        let mut buf = Vec::new();
        let status = if let Some(ver) = tools.rg {
            self.exec(&rg_script(&root_s, q, ver), cancel, &mut |b| {
                buf.extend_from_slice(b);
                drain_records(&mut buf, b'\n', |line| {
                    if let Some(ev) = parse_rg_line(line) {
                        emit(ev);
                    }
                });
            })?
        } else if tools.grep {
            self.exec(
                &grep_script(&root_s, q, tools.grep_pcre),
                cancel,
                &mut |b| {
                    buf.extend_from_slice(b);
                    drain_grep_records(&mut buf, |path, line_no, text| {
                        let mut ranges = Vec::new();
                        let _ = matcher.find_iter(text.as_bytes(), |m| {
                            ranges.push(m.start()..m.end());
                            true
                        });
                        let (line, ranges) = clip_hit(text, ranges);
                        emit(SearchEvent::Match {
                            path: strip_dot(path),
                            line_no,
                            line,
                            ranges,
                        });
                    });
                },
            )?
        } else {
            bail!(
                "grep {}: neither rg nor grep is available on the host",
                self.dest.alias
            );
        };
        check_status(status, &self.ctx("grep", &root))
    }

    fn reconnect(&self) -> Result<()> {
        let sess = self.new_session()?;
        let conn = sftp_conn(sess).with_context(|| format!("sftp {}", self.dest.alias))?;
        *self.conn.write().unwrap() = Arc::new(conn);
        self.exec_pool.lock().unwrap().clear();
        Ok(())
    }
}

// ---- connection setup ----------------------------------------------------

/// Applies `~/.ssh/config`: explicit user/port/jump > config > $USER / 22.
fn resolve(t: &RemoteTarget, opts: &ConnectOpts) -> Dest {
    let params = dirs::home_dir()
        .and_then(|h| fs::File::open(h.join(".ssh/config")).ok())
        .and_then(|f| {
            let rules = ssh2_config::ParseRule::ALLOW_UNKNOWN_FIELDS
                | ssh2_config::ParseRule::ALLOW_UNSUPPORTED_FIELDS;
            ssh2_config::SshConfig::default()
                .parse(&mut BufReader::new(f), rules)
                .ok()
        })
        .map(|c| c.query(&t.host));
    let p = params.as_ref();
    let host = p
        .and_then(|p| p.host_name.clone())
        .map(|h| h.replace("%h", &t.host))
        .unwrap_or(t.host.clone());
    let user = t
        .user
        .clone()
        .or_else(|| p.and_then(|p| p.user.clone()))
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok())
        .unwrap_or_else(|| "root".into());
    Dest {
        alias: t.host.clone(),
        host,
        port: t.port.or_else(|| p.and_then(|p| p.port)).unwrap_or(22),
        user,
        cfg_keys: p.and_then(|p| p.identity_file.clone()).unwrap_or_default(),
        jump: opts
            .jump
            .clone()
            .or_else(|| p.and_then(|p| p.proxy_jump.as_ref()).map(|j| j.join(",")))
            .and_then(|j| jump::hops(&j)),
    }
}

/// `RCP_KNOWN_HOSTS` overrides `~/.ssh/known_hosts` (used by tests).
fn known_hosts_path() -> PathBuf {
    std::env::var_os("RCP_KNOWN_HOSTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".ssh/known_hosts")
        })
}

enum HostCheck<'a> {
    /// First connect: check known_hosts, ask about unknown hosts.
    KnownHosts(&'a Path),
    /// Reconnect: the key must equal the one accepted before.
    Pinned(&'a [u8]),
}

fn connect_tcp(d: &Dest) -> Result<TcpStream> {
    let addrs = (d.host.as_str(), d.port)
        .to_socket_addrs()
        .with_context(|| format!("resolve {}", d.host))?;
    let mut tcp = Err(io::Error::new(io::ErrorKind::NotFound, "no addresses"));
    for a in addrs {
        tcp = TcpStream::connect_timeout(&a, Duration::from_secs(10));
        if tcp.is_ok() {
            break;
        }
    }
    let tcp = tcp.with_context(|| format!("connect {}:{}", d.host, d.port))?;
    let _ = tcp.set_nodelay(true);
    Ok(tcp)
}

/// `jump` must be the one started for `d`. Through a jump the host name is
/// resolved on the last jump host, not here.
fn open_session(d: &Dest, jump: Option<&Jump>, check: HostCheck) -> Result<(Session, Vec<u8>)> {
    let mut sess = Session::new()?;
    match jump {
        Some(j) => sess.set_tcp_stream(j.tunnel(
            &d.host,
            d.port,
            matches!(check, HostCheck::KnownHosts(_)),
        )?),
        None => sess.set_tcp_stream(connect_tcp(d)?),
    }
    sess.set_timeout(30_000);
    let kh_lines = match check {
        HostCheck::KnownHosts(p) => fs::read_to_string(p).unwrap_or_default(),
        HostCheck::Pinned(_) => String::new(),
    };
    // Prefer key types known_hosts already has for this host (or the pinned
    // key's type), as OpenSSH does; otherwise libssh2 may negotiate another
    // type and the key would look changed.
    let known = match check {
        HostCheck::KnownHosts(_) => known_types(&sess, &kh_lines, &d.host, d.port),
        HostCheck::Pinned(k) => vec![key_type(k)],
    };
    if !known.is_empty() {
        let supported = sess.supported_algs(MethodType::HostKey)?;
        let mut pref: Vec<&str> = Vec::new();
        for t in &known {
            let algs: &[&str] = if t == "ssh-rsa" {
                &["rsa-sha2-512", "rsa-sha2-256", "ssh-rsa"]
            } else {
                &[t]
            };
            pref.extend(algs.iter().filter(|a| supported.contains(a)));
        }
        let rest: Vec<&str> = supported
            .iter()
            .copied()
            .filter(|a| !pref.contains(a))
            .collect();
        pref.extend(rest);
        let _ = sess.method_pref(MethodType::HostKey, &pref.join(","));
    }
    sess.handshake().with_context(|| {
        let via = d
            .jump
            .as_ref()
            .map(|j| format!(" via {}", j.join(",")))
            .unwrap_or_default();
        format!("ssh handshake with {}:{}{via}", d.host, d.port)
    })?;
    let (key, _) = sess
        .host_key()
        .ok_or_else(|| anyhow!("{}: no host key", d.host))?;
    let key = key.to_vec();
    match check {
        HostCheck::Pinned(k) if k == key.as_slice() => {}
        HostCheck::Pinned(_) => bail!(
            "host key for {} changed since connecting; refusing to reconnect",
            d.host
        ),
        HostCheck::KnownHosts(path) => verify_host(&sess, &kh_lines, path, d, &key)?,
    }
    Ok((sess, key))
}

/// Host spec as written in known_hosts.
fn host_spec(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// Usable known_hosts lines (markers like @revoked are skipped).
fn kh_entries(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('@'))
}

/// Checks one line at a time so a single unparsable line (e.g. a key type
/// libssh2 doesn't know) can't break the whole file.
fn check_line(
    sess: &Session,
    line: &str,
    host: &str,
    port: u16,
    key: &[u8],
) -> Option<CheckResult> {
    let mut kh = sess.known_hosts().ok()?;
    kh.read_str(line, KnownHostFileKind::OpenSSH).ok()?;
    Some(kh.check_port(host, port, key))
}

/// Key types recorded for `host`. libssh2 compares keys of any type, so a
/// dummy key reports "mismatch" exactly on the lines naming this host.
fn known_types(sess: &Session, kh: &str, host: &str, port: u16) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in kh_entries(kh) {
        if let Some(CheckResult::Mismatch) = check_line(sess, line, host, port, b"rcp-probe")
            && let Some(t) = line.split_whitespace().nth(1)
            && !out.iter().any(|o| o == t)
        {
            out.push(t.to_string());
        }
    }
    out
}

fn verify_host(sess: &Session, kh: &str, path: &Path, d: &Dest, key: &[u8]) -> Result<()> {
    let ktype = key_type(key);
    let mut changed = false;
    for line in kh_entries(kh) {
        match check_line(sess, line, &d.host, d.port, key) {
            Some(CheckResult::Match) => return Ok(()),
            Some(CheckResult::Mismatch)
                if line.split_whitespace().nth(1) == Some(ktype.as_str()) =>
            {
                changed = true
            }
            _ => {}
        }
    }
    let fp = sess
        .host_key_hash(HashType::Sha256)
        .map(|h| b64_encode(h, false))
        .unwrap_or_default();
    let spec = host_spec(&d.host, d.port);
    if changed {
        bail!(
            "HOST KEY FOR {spec} HAS CHANGED (now {ktype} SHA256:{fp}). Someone could be eavesdropping, or the \
             host key was replaced. Refusing to connect; fix {} if the change is expected.",
            path.display()
        );
    }
    eprintln!("The authenticity of host '{spec}' can't be established.");
    eprintln!("{ktype} key fingerprint is SHA256:{fp}.");
    eprint!("Are you sure you want to continue connecting (yes/no)? ");
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "yes" | "y") {
        bail!("host key for {spec} not accepted");
    }
    if let Err(e) = append_known_host(path, &spec, &ktype, key) {
        eprintln!("warning: could not update {}: {e}", path.display());
    }
    Ok(())
}

fn append_known_host(path: &Path, spec: &str, ktype: &str, key: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent()
        && !dir.exists()
    {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    let needs_nl = fs::read(path)
        .map(|b| b.last().is_some_and(|&c| c != b'\n'))
        .unwrap_or(false);
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let nl = if needs_nl { "\n" } else { "" };
    writeln!(f, "{nl}{spec} {ktype} {}", b64_encode(key, true))?;
    Ok(())
}

/// The key type name is the first string in an SSH public key blob.
fn key_type(blob: &[u8]) -> String {
    let n = blob
        .get(..4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize)
        .unwrap_or(0);
    blob.get(4..4 + n)
        .map(|t| String::from_utf8_lossy(t).into_owned())
        .unwrap_or_else(|| "unknown".into())
}

fn sftp_conn(sess: Session) -> Result<Conn> {
    let sftp = sess.sftp()?;
    sess.set_timeout(SFTP_TIMEOUT_MS);
    Ok(Conn { _sess: sess, sftp })
}

// ---- authentication --------------------------------------------------------

/// Answers keyboard-interactive prompts with a known password, or asks.
struct Kbd {
    answer: Option<String>,
}

impl KeyboardInteractivePrompt for Kbd {
    fn prompt<'a>(
        &mut self,
        _user: &str,
        instructions: &str,
        prompts: &[Prompt<'a>],
    ) -> Vec<String> {
        if let Some(a) = &self.answer {
            return prompts.iter().map(|_| a.clone()).collect();
        }
        if !instructions.is_empty() {
            eprintln!("{instructions}");
        }
        let answers: Vec<String> = prompts
            .iter()
            .map(|p| rpassword::prompt_password(p.text.as_ref()).unwrap_or_default())
            .collect();
        self.answer = answers.first().cloned();
        answers
    }
}

fn try_agent(sess: &Session, user: &str) -> bool {
    let Ok(mut agent) = sess.agent() else {
        return false;
    };
    if agent.connect().is_err() || agent.list_identities().is_err() {
        return false;
    }
    let ok = agent
        .identities()
        .unwrap_or_default()
        .iter()
        .any(|id| agent.userauth(user, id).is_ok());
    let _ = agent.disconnect();
    ok
}

fn try_key(sess: &Session, user: &str, key: &Path, passphrase: Option<&str>) -> bool {
    let pubkey = PathBuf::from(format!("{}.pub", key.display()));
    let pubkey = pubkey.exists().then_some(pubkey);
    sess.userauth_pubkey_file(user, pubkey.as_deref(), key, passphrase)
        .is_ok()
}

fn authenticate(sess: &Session, d: &Dest, opts: &ConnectOpts) -> Result<Cred> {
    let user = d.user.as_str();
    let methods = match sess.auth_methods(user) {
        Ok(m) => m.to_string(),
        Err(_) if sess.authenticated() => return Ok(Cred::None),
        Err(e) => return Err(e).context("ssh auth"),
    };
    if methods.contains("publickey") {
        if try_agent(sess, user) {
            return Ok(Cred::Agent);
        }
        let home = dirs::home_dir().unwrap_or_default();
        let mut keys: Vec<PathBuf> = opts
            .identity
            .iter()
            .map(|p| crate::cli::expand_tilde(p))
            .collect();
        keys.extend(d.cfg_keys.iter().cloned());
        keys.extend(
            ["id_ed25519", "id_ecdsa", "id_rsa"]
                .iter()
                .map(|k| home.join(".ssh").join(k)),
        );
        let mut seen = Vec::new();
        for key in keys {
            if seen.contains(&key) || !key.is_file() {
                continue;
            }
            seen.push(key.clone());
            if try_key(sess, user, &key, None) {
                return Ok(Cred::Key {
                    path: key,
                    passphrase: None,
                });
            }
            if !key_is_encrypted(&key) {
                continue;
            }
            for _ in 0..3 {
                let pp = rpassword::prompt_password(format!(
                    "Enter passphrase for key '{}': ",
                    key.display()
                ))?;
                if pp.is_empty() {
                    break;
                }
                if try_key(sess, user, &key, Some(&pp)) {
                    return Ok(Cred::Key {
                        path: key,
                        passphrase: Some(pp),
                    });
                }
            }
        }
    }
    for _ in 0..3 {
        if methods.contains("password") {
            let pw = rpassword::prompt_password(format!("{user}@{}'s password: ", d.host))?;
            if sess.userauth_password(user, &pw).is_ok() {
                return Ok(Cred::Password(pw));
            }
        } else if methods.contains("keyboard-interactive") {
            let mut kbd = Kbd { answer: None };
            if sess.userauth_keyboard_interactive(user, &mut kbd).is_ok() {
                return Ok(Cred::Keyboard(kbd.answer.unwrap_or_default()));
            }
        } else {
            break;
        }
        eprintln!("Permission denied, please try again.");
    }
    bail!(
        "authentication failed for {user}@{} (tried: {methods})",
        d.host
    )
}

/// Repeats a successful login without prompting.
fn reauth(sess: &Session, user: &str, cred: &Cred) -> Result<()> {
    let ok = match cred {
        Cred::None => sess.auth_methods(user).is_err() && sess.authenticated(),
        Cred::Agent => try_agent(sess, user),
        Cred::Key { path, passphrase } => try_key(sess, user, path, passphrase.as_deref()),
        Cred::Password(pw) => sess.userauth_password(user, pw).is_ok(),
        Cred::Keyboard(a) => sess
            .userauth_keyboard_interactive(
                user,
                &mut Kbd {
                    answer: Some(a.clone()),
                },
            )
            .is_ok(),
    };
    if !ok || !sess.authenticated() {
        bail!("re-authentication as {user} failed");
    }
    Ok(())
}

/// PEM keys say ENCRYPTED; OpenSSH-format keys name a cipher other than "none".
fn key_is_encrypted(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    if text.contains("ENCRYPTED") {
        return true;
    }
    let body: String = text.lines().filter(|l| !l.starts_with("-----")).collect();
    let Some(blob) = b64_decode(&body) else {
        return false;
    };
    let Some(rest) = blob.strip_prefix(b"openssh-key-v1\0") else {
        return false;
    };
    let n = rest
        .get(..4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize)
        .unwrap_or(0);
    rest.get(4..4 + n).is_some_and(|c| c != b"none")
}

// ---- helpers ---------------------------------------------------------------

fn blank_stat() -> ssh2::FileStat {
    ssh2::FileStat {
        size: None,
        uid: None,
        gid: None,
        perm: None,
        atime: None,
        mtime: None,
    }
}

fn entry_from(name: String, is_symlink: bool, st: &ssh2::FileStat) -> Entry {
    let kind = match st.perm.map(|p| p & 0o170000) {
        Some(0o040000) => Kind::Dir,
        Some(0o100000) => Kind::File,
        _ => Kind::Other,
    };
    Entry {
        name,
        kind,
        is_symlink,
        size: st.size.unwrap_or(0),
        modified: st.mtime.map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        mode: st.perm,
    }
}

/// Failure before the remote command started (channel open / exec), so a
/// stale pooled session can be replaced and the command retried.
#[derive(Debug)]
struct ExecStartError(ssh2::Error);

impl std::fmt::Display for ExecStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ExecStartError {}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

fn run_exec(
    sess: &Session,
    script: &str,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<Option<i32>> {
    sess.set_timeout(15_000);
    let start = || -> Result<ssh2::Channel, ssh2::Error> {
        let mut ch = sess.channel_session()?;
        // Discard stderr so it can't fill the window and stall stdout.
        ch.handle_extended_data(ExtendedData::Ignore)?;
        ch.exec(&format!("sh -c {}", quote(script)))?;
        // EOF on stdin: rg won't wait for it, a forced sftp-server exits.
        ch.send_eof()?;
        Ok(ch)
    };
    let mut ch = start().map_err(ExecStartError)?;
    sess.set_timeout(POLL_MS);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = ch.close();
            return Ok(None);
        }
        match ch.read(&mut buf) {
            Ok(0) if ch.eof() => break,
            Ok(n) => sink(&buf[..n]),
            Err(e) if is_timeout(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    loop {
        match ch.wait_close() {
            Ok(()) => break,
            Err(e) if matches!(e.code(), ErrorCode::Session(-9 | -37)) => {
                // LIBSSH2_ERROR_TIMEOUT / EAGAIN
                if cancel.load(Ordering::Relaxed) {
                    return Ok(None);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(ch.exit_status()?))
}

fn check_status(status: Option<i32>, what: &str) -> Result<()> {
    match status {
        Some(NO_ROOT) => bail!("{what}: no such directory"),
        Some(127) => bail!("{what}: remote command not found"),
        _ => Ok(()),
    }
}

/// Calls `f` for each complete `sep`-terminated record and keeps the rest.
fn drain_records(buf: &mut Vec<u8>, sep: u8, mut f: impl FnMut(&[u8])) {
    let mut start = 0;
    while let Some(i) = buf[start..].iter().position(|&b| b == sep) {
        f(&buf[start..start + i]);
        start += i + 1;
    }
    buf.drain(..start);
}

/// Parses `grep -rn --null` output: `path\0lineno:text\n`. Splitting at
/// NUL first keeps paths containing `:` intact.
fn drain_grep_records(buf: &mut Vec<u8>, mut f: impl FnMut(&str, u64, &str)) {
    let mut start = 0;
    while let Some(z) = buf[start..].iter().position(|&b| b == 0) {
        let Some(nl) = buf[start + z..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let path = String::from_utf8_lossy(&buf[start..start + z]);
        let rest = String::from_utf8_lossy(&buf[start + z + 1..start + z + nl]);
        if let Some((n, text)) = rest.split_once(':')
            && let Ok(n) = n.parse()
        {
            f(&path, n, text);
        }
        start += z + nl + 1;
    }
    buf.drain(..start);
}

fn strip_dot(p: &str) -> String {
    p.strip_prefix("./").unwrap_or(p).to_string()
}

fn json_text(v: &serde_json::Value) -> Option<String> {
    if let Some(t) = v["text"].as_str() {
        return Some(t.to_string());
    }
    // Non-UTF-8 data comes base64-encoded.
    let b = b64_decode(v["bytes"].as_str()?)?;
    Some(String::from_utf8_lossy(&b).into_owned())
}

/// One line of `rg --json`; only `match` messages produce events.
fn parse_rg_line(line: &[u8]) -> Option<SearchEvent> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    if v["type"] != "match" {
        return None;
    }
    let d = &v["data"];
    let path = json_text(&d["path"])?;
    let text = json_text(&d["lines"])?;
    let line_no = d["line_number"].as_u64()?;
    let ranges = d["submatches"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| Some(m["start"].as_u64()? as usize..m["end"].as_u64()? as usize))
                // Lossy decoding can shift offsets; drop ranges that no longer fit.
                .filter(|r| {
                    r.end <= text.len()
                        && text.is_char_boundary(r.start)
                        && text.is_char_boundary(r.end)
                })
                .collect()
        })
        .unwrap_or_default();
    let (line, ranges) = clip_hit(&text, ranges);
    Some(SearchEvent::Match {
        path: strip_dot(&path),
        line_no,
        line,
        ranges,
    })
}

fn build_matcher(q: &GrepQuery) -> Result<RegexMatcher> {
    Ok(RegexMatcherBuilder::new()
        .case_smart(q.case == CaseMode::Smart)
        .case_insensitive(q.case == CaseMode::Insensitive)
        .word(q.whole_word)
        .fixed_strings(!q.regex)
        .build(&q.pattern)?)
}

// ---- remote commands -------------------------------------------------------

fn quote(s: &str) -> String {
    shell_escape::unix::escape(Cow::Borrowed(s)).into_owned()
}

/// Prints `rcp-ok`, then `tool <name> <version line>` per search tool.
const DETECT_SCRIPT: &str = "echo rcp-ok; \
    for t in fd fdfind rg; do v=$(\"$t\" --version 2>/dev/null | head -n 1); [ -n \"$v\" ] && echo \"tool $t $v\"; done; \
    command -v grep >/dev/null 2>&1 && echo 'tool grep' && echo x | grep -qP 'x' 2>/dev/null && echo 'tool grep-P'; \
    exit 0";

fn parse_version(s: &str) -> (u32, u32) {
    let mut it = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

fn parse_tools(out: &str) -> Tools {
    let mut t = Tools {
        sh: out.lines().any(|l| l == "rcp-ok"),
        ..Default::default()
    };
    for l in out.lines() {
        let mut parts = l.splitn(3, ' ');
        if parts.next() != Some("tool") {
            continue;
        }
        let name = parts.next().unwrap_or("");
        let ver = parts.next().unwrap_or("");
        match name {
            // Some distros ship an unrelated `fd`; the real one says "fd x.y".
            "fd" | "fdfind"
                if t.fd.is_none() && matches!(ver.split(' ').next(), Some("fd" | "fdfind")) =>
            {
                t.fd = Some((name.to_string(), parse_version(ver)))
            }
            "rg" if ver.starts_with("ripgrep") => t.rg = Some(parse_version(ver)),
            "grep" => t.grep = true,
            "grep-P" => t.grep_pcre = true,
            _ => {}
        }
    }
    t
}

/// `cd` into the root so output paths come out relative to it.
fn cd_root(root: &str) -> String {
    format!("cd -- {} 2>/dev/null || exit {NO_ROOT}; ", quote(root))
}

fn fd_script(fd: &str, ver: (u32, u32), root: &str, o: WalkOpts) -> String {
    let mut s = cd_root(root) + "exec " + &quote(fd) + " --type f --color never -0 --exclude .git";
    if o.hidden {
        s += " --hidden";
    }
    if !o.respect_ignore {
        s += " --no-ignore";
    } else if ver >= (8, 6) {
        s += " --no-require-git";
    }
    s + " 2>/dev/null"
}

fn find_script(root: &str, o: WalkOpts) -> String {
    let prune = if o.hidden {
        "-name .git -prune"
    } else {
        "-name '.*' ! -name . -prune"
    };
    cd_root(root) + "exec find . " + prune + " -o -type f -print0 2>/dev/null"
}

fn rg_script(root: &str, q: &GrepQuery, ver: (u32, u32)) -> String {
    let mut a = vec!["rg", "--json", "--no-config"];
    if !q.regex {
        a.push("-F");
    }
    a.push(match q.case {
        CaseMode::Smart => "-S",
        CaseMode::Sensitive => "-s",
        CaseMode::Insensitive => "-i",
    });
    if q.whole_word {
        a.push("-w");
    }
    if q.hidden {
        a.push("--hidden");
    }
    if !q.respect_ignore {
        a.push("--no-ignore");
    } else if ver >= (12, 0) {
        a.push("--no-require-git");
    }
    let mut s = cd_root(root) + "exec " + &a.join(" ") + " -g '!.git'";
    for g in &q.globs {
        s += &format!(" -g {}", quote(g));
    }
    s + " -- " + &quote(&q.pattern) + " . 2>/dev/null"
}

fn grep_script(root: &str, q: &GrepQuery, pcre: bool) -> String {
    let mut s = cd_root(root) + "exec grep -rnI --null --color=never";
    s += if !q.regex {
        " -F"
    } else if pcre {
        " -P"
    } else {
        " -E"
    };
    let insensitive = match q.case {
        CaseMode::Smart => !q.pattern.chars().any(char::is_uppercase),
        CaseMode::Sensitive => false,
        CaseMode::Insensitive => true,
    };
    if insensitive {
        s += " -i";
    }
    if q.whole_word {
        s += " -w";
    }
    s += " --exclude-dir=.git";
    // grep matches only base names, so globs are mapped best-effort.
    for g in &q.globs {
        let (neg, g) = match g.strip_prefix('!') {
            Some(g) => (true, g),
            None => (false, g.as_str()),
        };
        let dir = g.trim_end_matches("/**").trim_end_matches('/');
        let base = dir.rsplit('/').next().unwrap_or(dir);
        if base.is_empty() || base == "**" {
            continue;
        }
        let flag = match (neg, dir.len() != g.len()) {
            (true, true) => "--exclude-dir",
            (true, false) => "--exclude",
            (false, _) => "--include",
        };
        s += &format!(" {flag}={}", quote(base));
    }
    // `.[!/]*` rather than `.*`, which would also exclude the `.` start dir.
    // Not `.?*`: BSD grep also matches globs against the whole `./sub/path`,
    // where `?*` would swallow the slash and exclude everything.
    if !q.hidden {
        s += " --exclude='.[!/]*' --exclude-dir='.[!/]*'";
    }
    s + " -e " + &quote(&q.pattern) + " . 2>/dev/null"
}

// ---- base64 ----------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
        if pad {
            out.push_str(&"=="[..3 - c.len()]);
        }
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for s in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            assert_eq!(b64_decode(&b64_encode(s, true)).unwrap(), s);
        }
        assert_eq!(b64_encode(b"foobar", true), "Zm9vYmFy");
        assert_eq!(b64_encode(b"fooba", true), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foob", false), "Zm9vYg");
        assert_eq!(key_type(b"\0\0\0\x0bssh-ed25519\0\0"), "ssh-ed25519");
    }

    #[test]
    fn rg_json_lines() {
        let line = br#"{"type":"match","data":{"path":{"text":"./src/a:b.rs"},"lines":{"text":"let x = \"it's\";\n"},"line_number":7,"absolute_offset":10,"submatches":[{"match":{"text":"it's"},"start":9,"end":13}]}}"#;
        match parse_rg_line(line) {
            Some(SearchEvent::Match {
                path,
                line_no,
                line,
                ranges,
            }) => {
                assert_eq!(path, "src/a:b.rs");
                assert_eq!(line_no, 7);
                assert_eq!(line, "let x = \"it's\";");
                assert_eq!(&line[ranges[0].clone()], "it's");
            }
            other => panic!("{other:?}"),
        }
        // Non-UTF-8 path and line arrive as base64 bytes.
        let line = format!(
            r#"{{"type":"match","data":{{"path":{{"bytes":"{}"}},"lines":{{"bytes":"{}"}},"line_number":1,"submatches":[{{"start":0,"end":3}}]}}}}"#,
            b64_encode(b"./x\xff", true),
            b64_encode(b"abc\xff\n", true)
        );
        match parse_rg_line(line.as_bytes()) {
            Some(SearchEvent::Match {
                path, line, ranges, ..
            }) => {
                assert_eq!(path, "x\u{fffd}");
                assert_eq!(&line[ranges[0].clone()], "abc");
            }
            other => panic!("{other:?}"),
        }
        assert!(parse_rg_line(br#"{"type":"begin","data":{"path":{"text":"a"}}}"#).is_none());
        assert!(parse_rg_line(br#"{"type":"summary","data":{}}"#).is_none());
        assert!(parse_rg_line(b"garbage").is_none());
    }

    #[test]
    fn grep_null_output() {
        let mut buf = b"./dir/a:b.txt\x0012:x: y\nsrc/m.rs\x003:fn main\nsrc/par".to_vec();
        let mut got = Vec::new();
        drain_grep_records(&mut buf, |p, n, t| {
            got.push((strip_dot(p), n, t.to_string()))
        });
        assert_eq!(
            got,
            vec![
                ("dir/a:b.txt".into(), 12, "x: y".into()),
                ("src/m.rs".into(), 3, "fn main".into())
            ]
        );
        assert_eq!(buf, b"src/par");
        buf.extend_from_slice(b"tial\x001:z\n");
        got.clear();
        drain_grep_records(&mut buf, |p, n, t| {
            got.push((p.to_string(), n, t.to_string()))
        });
        assert_eq!(got, vec![("src/partial".into(), 1, "z".into())]);
        assert!(buf.is_empty());
    }

    #[test]
    fn nul_records() {
        let mut buf = b"./a\0b/c\0d".to_vec();
        let mut got = Vec::new();
        drain_records(&mut buf, 0, |r| {
            got.push(strip_dot(&String::from_utf8_lossy(r)))
        });
        assert_eq!(got, vec!["a", "b/c"]);
        assert_eq!(buf, b"d");
    }

    /// Runs a script through `sh -c '<quoted>'` locally, as exec would.
    fn sh(script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("sh -c {}", quote(script)))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn quoting_survives_the_shell() {
        let q = GrepQuery {
            pattern: r#"it's a "test" $HOME `x` \n"#.into(),
            ..Default::default()
        };
        let s = rg_script("/tmp/dir with 'quote", &q, (14, 1));
        assert!(
            s.starts_with(
                r#"cd -- '/tmp/dir with '\''quote' 2>/dev/null || exit 99; exec rg --json"#
            ),
            "{s}"
        );
        // printf stands in for rg: the pattern must arrive as one literal arg.
        let echo = format!("printf '%s\\n' {}", quote(&q.pattern));
        assert_eq!(sh(&echo), format!("{}\n", q.pattern));
    }

    #[test]
    fn rg_and_grep_args() {
        let q = GrepQuery {
            pattern: "Foo bar".into(),
            regex: false,
            case: CaseMode::Smart,
            whole_word: true,
            hidden: false,
            respect_ignore: true,
            globs: vec!["*.rs".into(), "!target/**".into(), "!*.min.js".into()],
        };
        assert_eq!(
            rg_script("/r", &q, (14, 1)),
            "cd -- /r 2>/dev/null || exit 99; exec rg --json --no-config -F -S -w --no-require-git -g '!.git' \
             -g '*.rs' -g ''\\!'target/**' -g ''\\!'*.min.js' -- 'Foo bar' . 2>/dev/null"
        );
        assert_eq!(
            grep_script("/r", &q, true),
            "cd -- /r 2>/dev/null || exit 99; exec grep -rnI --null --color=never -F -w --exclude-dir=.git \
             --include='*.rs' --exclude-dir=target --exclude='*.min.js' --exclude='.[!/]*' --exclude-dir='.[!/]*' \
             -e 'Foo bar' . 2>/dev/null"
        );
        let q = GrepQuery {
            pattern: "a|b".into(),
            regex: true,
            hidden: true,
            globs: vec![],
            ..q
        };
        assert_eq!(
            grep_script("/r", &q, false),
            "cd -- /r 2>/dev/null || exit 99; exec grep -rnI --null --color=never -E -i -w --exclude-dir=.git \
             -e 'a|b' . 2>/dev/null"
        );
    }

    #[test]
    fn walk_commands() {
        let o = WalkOpts {
            hidden: false,
            respect_ignore: true,
        };
        assert_eq!(
            fd_script("fdfind", (8, 3), "/a b", o),
            "cd -- '/a b' 2>/dev/null || exit 99; exec fdfind --type f --color never -0 --exclude .git 2>/dev/null"
        );
        let o = WalkOpts {
            hidden: true,
            respect_ignore: false,
        };
        assert_eq!(
            fd_script("fd", (10, 0), "/a", o),
            "cd -- /a 2>/dev/null || exit 99; exec fd --type f --color never -0 --exclude .git --hidden --no-ignore 2>/dev/null"
        );
        assert_eq!(
            find_script(
                "/a",
                WalkOpts {
                    hidden: false,
                    respect_ignore: true
                }
            ),
            "cd -- /a 2>/dev/null || exit 99; exec find . -name '.*' ! -name . -prune -o -type f -print0 2>/dev/null"
        );
    }

    #[test]
    fn local_scripts_behave() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        fs::create_dir_all(p.join(".git")).unwrap();
        fs::create_dir_all(p.join(".hid")).unwrap();
        fs::create_dir_all(p.join("s p")).unwrap();
        fs::write(p.join(".git/HEAD"), "it's x").unwrap();
        fs::write(p.join(".hid/h"), "it's x").unwrap();
        fs::write(p.join("s p/a:b"), "1\nit's x\n").unwrap();
        let root = p.to_str().unwrap();
        let mut f: Vec<String> = sh(&find_script(root, WalkOpts::default()))
            .split('\0')
            .map(strip_dot)
            .collect();
        f.sort();
        assert_eq!(f, vec!["", "s p/a:b"]);
        let mut f: Vec<String> = sh(&find_script(
            root,
            WalkOpts {
                hidden: true,
                respect_ignore: false,
            },
        ))
        .split('\0')
        .map(strip_dot)
        .collect();
        f.sort();
        assert_eq!(f, vec!["", ".hid/h", "s p/a:b"]);
        let q = GrepQuery {
            pattern: "it's".into(),
            ..Default::default()
        };
        let mut buf = sh(&grep_script(root, &q, false)).into_bytes();
        let mut got = Vec::new();
        drain_grep_records(&mut buf, |p, n, t| {
            got.push((strip_dot(p), n, t.to_string()))
        });
        assert_eq!(got, vec![("s p/a:b".to_string(), 2, "it's x".to_string())]);
    }

    #[test]
    fn tools_parsing() {
        let t =
            parse_tools("rcp-ok\ntool fd fd 10.3.0\ntool rg ripgrep 14.1.0 (rev abc)\ntool grep\n");
        assert!(t.sh && t.grep && !t.grep_pcre);
        assert_eq!(t.fd, Some(("fd".into(), (10, 3))));
        assert_eq!(t.rg, Some((14, 1)));
        let t = parse_tools(
            "rcp-ok\ntool fd fdclone 3.01\ntool fdfind fdfind 8.3.1\ntool grep\ntool grep-P\n",
        );
        assert_eq!(t.fd, Some(("fdfind".into(), (8, 3))));
        assert!(t.grep_pcre && t.rg.is_none());
        assert!(!parse_tools("").sh);
    }

    #[test]
    fn detect_script_runs() {
        let t = parse_tools(&sh(DETECT_SCRIPT));
        assert!(t.sh && t.grep);
    }

    // ---- integration: RCP_TEST_SSH=user@127.0.0.1:2299 RCP_TEST_SSH_KEY=... ----
    // (RCP_TEST_SSH_JUMP=... runs the same tests through a jump host.)

    fn connect_test() -> Option<SftpFs> {
        let spec = std::env::var("RCP_TEST_SSH").ok()?;
        let (user, rest) = spec
            .split_once('@')
            .map(|(u, r)| (Some(u.to_string()), r))
            .unwrap_or((None, &spec));
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()),
            None => (rest.to_string(), None),
        };
        let opts = ConnectOpts {
            identity: std::env::var_os("RCP_TEST_SSH_KEY").map(PathBuf::from),
            jump: std::env::var("RCP_TEST_SSH_JUMP").ok(),
        };
        Some(SftpFs::connect(&RemoteTarget { user, host, port }, &opts).expect("connect"))
    }

    fn collect(f: impl FnOnce(Emit)) -> Vec<SearchEvent> {
        let out = Mutex::new(Vec::new());
        f(&|e| out.lock().unwrap().push(e));
        out.into_inner().unwrap()
    }

    fn sorted_paths(ev: &[SearchEvent]) -> Vec<String> {
        let mut v: Vec<String> = ev
            .iter()
            .filter_map(|e| match e {
                SearchEvent::Path(p) | SearchEvent::Match { path: p, .. } => Some(p.clone()),
                SearchEvent::Error(_) => None,
            })
            .collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn remote_backend() {
        let Some(fs) = connect_test() else {
            eprintln!("RCP_TEST_SSH not set; skipping");
            return;
        };
        let base = fs.canonicalize(Path::new("")).unwrap();
        let root = base.join(format!("rcp-test-{}", std::process::id()));
        fs.mkdir(&root).unwrap();
        let cancel = AtomicBool::new(false);

        // Basic file ops.
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let f = root.join("big file.bin");
        {
            let mut w = fs.open_write(&f).unwrap();
            w.write_all(&data).unwrap();
            w.flush().unwrap();
        }
        let mut back = Vec::new();
        fs.open_read(&f).unwrap().read_to_end(&mut back).unwrap();
        assert!(back == data, "roundtrip");
        let e = fs.stat(&f).unwrap();
        assert_eq!(
            (e.kind, e.size, e.is_symlink, e.name.as_str()),
            (Kind::File, 3_000_000, false, "big file.bin")
        );
        fs.set_mode(&f, 0o600).unwrap();
        assert_eq!(fs.stat(&f).unwrap().mode.unwrap() & 0o777, 0o600);
        let t = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs.set_mtime(&f, t).unwrap();
        assert_eq!(fs.stat(&f).unwrap().modified, Some(t));
        assert!(fs.exists(&f).unwrap());
        assert!(!fs.exists(&root.join("nope")).unwrap());

        // Rename onto an existing target fails on OpenSSH; plain rename works.
        let g = root.join("g.txt");
        fs.open_write(&g).unwrap().write_all(b"g").unwrap();
        let renamed = fs.rename(&f, &g);
        if renamed.is_err() {
            fs.remove_file(&g).unwrap();
            fs.rename(&f, &g).unwrap();
        }
        assert_eq!(fs.stat(&g).unwrap().size, 3_000_000);

        // Tree for walk/grep, plus symlinks.
        let tree_root = root.join("tree");
        fs.mkdir(&tree_root).unwrap();
        for d in ["src", "src/sub", ".hid", ".git", "dir:colon"] {
            fs.mkdir(&tree_root.join(d)).unwrap();
        }
        for (p, c) in [
            ("src/main.rs", "fn main() {\n    copy_entry(\"it's\");\n}\n"),
            ("src/sub/lib.rs", "pub fn Copy_Entry() {}\n"),
            (".hid/x.rs", "copy_entry\n"),
            (".git/HEAD", "copy_entry\n"),
            ("dir:colon/a:b.txt", "x\ncopy_entry here\n"),
        ] {
            fs.open_write(&tree_root.join(p))
                .unwrap()
                .write_all(c.as_bytes())
                .unwrap();
        }
        // ssh2 has no symlink on the Backend; make them via exec.
        let mut sink = |_: &[u8]| {};
        let mk = format!(
            "cd {} && ln -s src/main.rs link.rs && ln -s missing dangling && ln -s src dirlink",
            quote(&tree_root.to_string_lossy())
        );
        assert_eq!(fs.exec(&mk, &cancel, &mut sink).unwrap(), Some(0));

        let list = fs.list(&tree_root).unwrap();
        let get = |n: &str| list.iter().find(|e| e.name == n).unwrap().clone();
        let mut names: Vec<_> = list.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                ".git",
                ".hid",
                "dangling",
                "dir:colon",
                "dirlink",
                "link.rs",
                "src"
            ]
        );
        assert!(get("link.rs").is_symlink && get("link.rs").kind == Kind::File);
        assert!(get("dirlink").is_symlink && get("dirlink").is_dir());
        assert!(get("dangling").is_symlink && get("dangling").kind == Kind::Other);
        assert!(fs.exists(&tree_root.join("dangling")).unwrap());
        let st = fs.stat(&tree_root.join("link.rs")).unwrap();
        assert!(st.is_symlink && st.kind == Kind::File && st.size > 0);

        // Relative paths resolve from home.
        let rel = root.strip_prefix(&base).unwrap();
        assert_eq!(fs.stat(rel).unwrap().kind, Kind::Dir);

        let expect_walk = vec!["dir:colon/a:b.txt", "src/main.rs", "src/sub/lib.rs"];
        let expect_walk_hidden = vec![
            ".hid/x.rs",
            "dir:colon/a:b.txt",
            "src/main.rs",
            "src/sub/lib.rs",
        ];
        let q = GrepQuery {
            pattern: "copy_entry".into(),
            regex: true,
            respect_ignore: true,
            ..Default::default()
        };
        let qfix = GrepQuery {
            pattern: "(\"it's\")".into(),
            regex: false,
            ..q.clone()
        };
        let detected = fs.tools();
        eprintln!("detected tools: {detected:?}");
        let mut variants = vec![
            ("detected", detected.clone()),
            (
                "find+grep",
                Tools {
                    fd: None,
                    rg: None,
                    ..detected.clone()
                },
            ),
            (
                "grep -E",
                Tools {
                    fd: None,
                    rg: None,
                    grep_pcre: false,
                    ..detected.clone()
                },
            ),
            ("sftp only", Tools::default()),
        ];
        // fd is often outside sshd's default PATH; RCP_TEST_FD names it.
        if let Ok(fd) = std::env::var("RCP_TEST_FD") {
            variants.push((
                "fd",
                Tools {
                    fd: Some((fd, (10, 0))),
                    ..detected.clone()
                },
            ));
        }
        for (name, tools) in variants {
            *fs.tools.lock().unwrap() = Some(tools.clone());
            let o = WalkOpts {
                hidden: false,
                respect_ignore: true,
            };
            let ev = collect(|e| fs.walk_files(&tree_root, o, e, &cancel).unwrap());
            assert_eq!(sorted_paths(&ev), expect_walk, "{name}");
            let o = WalkOpts {
                hidden: true,
                respect_ignore: false,
            };
            let ev = collect(|e| fs.walk_files(&tree_root, o, e, &cancel).unwrap());
            assert_eq!(sorted_paths(&ev), expect_walk_hidden, "{name}");
            if !tools.grep {
                assert!(fs.grep(&tree_root, &q, &|_| {}, &cancel).is_err());
                continue;
            }
            // Smart case: lowercase pattern also matches Copy_Entry.
            let ev = collect(|e| fs.grep(&tree_root, &q, e, &cancel).unwrap());
            assert_eq!(
                sorted_paths(&ev),
                vec!["dir:colon/a:b.txt", "src/main.rs", "src/sub/lib.rs"],
                "{name}"
            );
            for e in &ev {
                if let SearchEvent::Match {
                    path,
                    line_no,
                    line,
                    ranges,
                } = e
                {
                    assert_eq!(
                        line[ranges[0].clone()].to_lowercase(),
                        "copy_entry",
                        "{name} {path}"
                    );
                    if path == "dir:colon/a:b.txt" {
                        assert_eq!(*line_no, 2);
                    }
                }
            }
            let qs = GrepQuery {
                case: CaseMode::Sensitive,
                hidden: true,
                ..q.clone()
            };
            let ev = collect(|e| fs.grep(&tree_root, &qs, e, &cancel).unwrap());
            assert_eq!(
                sorted_paths(&ev),
                vec![".hid/x.rs", "dir:colon/a:b.txt", "src/main.rs"],
                "{name}"
            );
            let ev = collect(|e| fs.grep(&tree_root, &qfix, e, &cancel).unwrap());
            assert_eq!(sorted_paths(&ev), vec!["src/main.rs"], "{name}");
            let qg = GrepQuery {
                globs: vec!["!src/**".into()],
                ..q.clone()
            };
            let ev = collect(|e| fs.grep(&tree_root, &qg, e, &cancel).unwrap());
            assert_eq!(sorted_paths(&ev), vec!["dir:colon/a:b.txt"], "{name}");
            assert!(
                fs.grep(&root.join("missing"), &q, &|_| {}, &cancel)
                    .is_err(),
                "{name}"
            );
        }
        *fs.tools.lock().unwrap() = Some(detected);

        // Encrypted key with a stored passphrase, as reconnect would use it.
        if let (Ok(key), Ok(pp)) = (
            std::env::var("RCP_TEST_SSH_ENC_KEY"),
            std::env::var("RCP_TEST_SSH_ENC_PASS"),
        ) {
            assert!(key_is_encrypted(Path::new(&key)));
            let (sess, _) =
                open_session(&fs.dest, fs.jump.as_ref(), HostCheck::Pinned(&fs.host_key)).unwrap();
            let cred = Cred::Key {
                path: key.into(),
                passphrase: Some(pp),
            };
            reauth(&sess, &fs.dest.user, &cred).unwrap();
        }

        // Throughput over loopback (rough sanity check, not a benchmark).
        let big: Vec<u8> = (0..64u32 << 20).map(|i| (i % 253) as u8).collect();
        let t0 = std::time::Instant::now();
        let mut w = fs.open_write(&f).unwrap();
        for c in big.chunks(IO_BUF) {
            w.write_all(c).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        let wt = t0.elapsed();
        let t0 = std::time::Instant::now();
        let mut r = fs.open_read(&f).unwrap();
        let (mut n, mut chunk) = (0, vec![0u8; IO_BUF]);
        loop {
            match r.read(&mut chunk).unwrap() {
                0 => break,
                k => n += k,
            }
        }
        let rt = t0.elapsed();
        assert_eq!(n, big.len());
        let mb = |d: Duration| 64.0 / d.as_secs_f64();
        eprintln!("64 MiB write {:.0} MiB/s, read {:.0} MiB/s", mb(wt), mb(rt));
        fs.remove_file(&f).unwrap();

        // Cancel stops a long-running command promptly.
        let cancel_now = AtomicBool::new(false);
        let t0 = std::time::Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                cancel_now.store(true, Ordering::Relaxed);
            });
            assert_eq!(fs.exec("sleep 30", &cancel_now, &mut |_| {}).unwrap(), None);
        });
        assert!(t0.elapsed() < Duration::from_secs(3));

        // Concurrent use: listing while a transfer runs, then reconnect.
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut r = fs.open_read(&g).unwrap();
                let mut sink = Vec::new();
                r.read_to_end(&mut sink).unwrap();
                assert_eq!(sink.len(), 3_000_000);
            });
            for _ in 0..20 {
                assert!(!fs.list(&tree_root).unwrap().is_empty());
            }
        });
        fs.reconnect().unwrap();
        assert!(fs.exists(&g).unwrap());

        // Cleanup with Backend calls only.
        fn rm(fs: &SftpFs, p: &Path) {
            for e in fs.list(p).unwrap() {
                let c = p.join(&e.name);
                if e.is_dir() && !e.is_symlink {
                    rm(fs, &c)
                } else {
                    fs.remove_file(&c).unwrap()
                }
            }
            fs.remove_dir(p).unwrap();
        }
        rm(&fs, &root);
        assert!(!fs.exists(&root).unwrap());
    }
}
