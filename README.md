# rcp

A two-pane terminal file copier. The left pane is your local filesystem; the right pane is another local directory or a remote host over SSH. Browse both sides, preview files, copy and delete, and search by file name (fzf-style) or by content (rg-style), locally or on the remote.

```
┏ Local: /home/ray/photos ━━━━━━━━━━┓┌ ray@nas: /volume1/photos ────────┐
┃ ..                          <dir> ┃│ ..                         <dir> │
┃ 2025/                       <dir> ┃│ 2024/                      <dir> │
┃ holiday.jpg               2.4 MiB ┃│ index.txt                  812 B │
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛└──────────────────────────────────┘
 Copying holiday.jpg ████████░░░░  68%  1.6 MiB / 2.4 MiB  4.1 MiB/s
 Tab switch · Enter open/preview · Space copy · d delete · / find · ^F grep
```

## Install

Release binaries are built for macOS (one universal binary for Apple silicon and Intel) and for Linux on x86_64 and aarch64. The Linux binaries are statically linked against musl, so one binary runs on any distribution. OpenSSL, libssh2 and zlib are compiled into every binary, so nothing else needs to be installed.

### Homebrew

```sh
brew install undo-git-pull/tap/rcp
```

This works on macOS and on Linux with Homebrew.

### Manual download

Download the archive for your platform from the [releases page](https://github.com/undo-git-pull/rcp/releases):

| Platform | Archive |
|---|---|
| macOS | `rcp-macos-universal.tar.gz` |
| Linux x86_64 | `rcp-linux-x86_64.tar.gz` |
| Linux aarch64 | `rcp-linux-aarch64.tar.gz` |

Each archive has a `.sha256` file next to it. Extract `rcp` and put it somewhere on your `PATH`, such as `~/.local/bin`. The macOS binary is not notarized by Apple, so macOS blocks a copy downloaded through a browser. Clear the quarantine flag first:

```sh
xattr -d com.apple.quarantine rcp
```

Homebrew doesn't need this step.

### From source

```sh
cargo install --git https://github.com/undo-git-pull/rcp
```

This needs the build requirements below.

## Building

Requirements:

- Rust 1.88 or newer (the crate uses edition 2024). Install with [rustup](https://rustup.rs).
- A C compiler, `make` and `perl`. The build compiles OpenSSL, libssh2 and zlib from source and links them statically, so no OpenSSL development package is needed.
  - Debian/Ubuntu: `sudo apt install build-essential perl`
  - Fedora: `sudo dnf install gcc make perl`
  - macOS: Xcode command line tools (`xcode-select --install`)

The first build takes about a minute longer than later ones while OpenSSL compiles. Cargo caches the result.

Build and install:

```sh
cargo build --release          # binary at target/release/rcp
cargo install --path .         # or install it into ~/.cargo/bin
```

Run the tests:

```sh
cargo test
```

The SSH integration test is skipped unless you point it at a server you can log into with a key:

```sh
RCP_TEST_SSH=user@host:22 RCP_TEST_SSH_KEY=~/.ssh/id_ed25519 \
RCP_KNOWN_HOSTS=/tmp/test_known_hosts cargo test
```

`RCP_KNOWN_HOSTS` makes rcp read and write that file instead of `~/.ssh/known_hosts`, so tests don't touch your real one. Add `RCP_TEST_SSH_JUMP=user@jumphost` to run the same test through a jump host.

### Formatting

Format the code with rustfmt before committing:

```sh
cargo fmt                      # rewrite files in place
cargo fmt --check              # only report files that need formatting
```

### Releasing

1. Set `version` in `Cargo.toml`, run `cargo build` so `Cargo.lock` picks it up, and commit both.
2. Tag and push:

   ```sh
   git tag v<version> && git push origin v<version>
   ```

   The [release workflow](.github/workflows/release.yml) checks that the tag matches `Cargo.toml` and runs the tests. It then builds the macOS universal binary and the two static Linux binaries (with [cross](https://github.com/cross-rs/cross)) and publishes them to a GitHub release with their `.sha256` files.

3. When the release is published, update the Homebrew formula from this repository:

   ```sh
   scripts/update-formula.sh v<version> ../homebrew-tap
   ```

   The script downloads each release archive, checks it against its published checksum, and writes `Formula/rcp.rb` from `packaging/homebrew/rcp.rb.in`. It doesn't touch git, so review the diff in the tap, then commit and push.

A local musl build is only needed to debug the Linux release build. Run `cross build --release --target x86_64-unknown-linux-musl` (needs Docker or Podman). A plain `cargo build` for the musl target needs `musl-gcc` from the `musl-tools` package and the kernel headers.

## Usage

```
rcp [OPTIONS] [LEFT] [RIGHT]

  LEFT    Local directory for the left pane (default: current directory)
  RIGHT   A local path, or an scp-style remote (default: same as LEFT)

  -p, --port <PORT>          SSH port (otherwise from ~/.ssh/config, then 22)
  -i, --identity <FILE>      SSH private key
  -J, --jump <HOSTS>         Jump hosts, comma-separated [user@]host[:port], as for ssh -J
      --hidden               Show dotfiles on startup
```

Remote locations use scp syntax: `user@host:/path`, `host:/path`, `host:relative/path` or `host:` for the remote home directory. IPv6 addresses go in brackets: `user@[::1]:/tmp`. If a local path with that name exists, it wins.

Examples:

```sh
rcp                                   # both panes in the current directory
rcp ~/photos /mnt/backup              # local ↔ local
rcp ~/photos nas:/volume1/photos      # local ↔ remote, using an ~/.ssh/config alias
rcp . ray@10.0.0.5:/srv/www -p 2222 -i ~/.ssh/deploy_key
```

### SSH

- Host aliases in `~/.ssh/config` are resolved (`HostName`, `User`, `Port`, `IdentityFile`, `ProxyJump`). `ProxyCommand` is not supported.
- Jump hosts come from `-J` or the config's `ProxyJump`, and are handled by the OpenSSH client (`ssh` must be on `PATH`; Unix only). rcp keeps one control connection to the last jump host for the session, so jump-host logins, including passwords, happen once, before the interface opens. Authentication to the destination itself works as below. If the jump connection drops, `r` restarts it without prompting, which needs ssh-agent or a key without a passphrase.
- Authentication tries ssh-agent, then `-i`, then the config's `IdentityFile`s, then `~/.ssh/id_ed25519`, `id_ecdsa` and `id_rsa`, then a password prompt. Prompts happen before the interface opens.
- Host keys are checked against `~/.ssh/known_hosts`. For an unknown host you're shown the fingerprint and asked to confirm; a changed key is refused.
- If the connection drops, press `r` to reconnect.

## Keys

### Main view

| Key | Action |
|---|---|
| `Tab` | Switch pane |
| `↑` `↓` / `j` `k` | Move |
| `PgUp` `PgDn`, `g` `G` | Page / jump to top or bottom |
| `Enter` | Open directory (`..` goes up); on a file, toggle preview |
| `→` / `l` | Open directory |
| `←` / `h` / `Backspace` | Parent directory |
| `Space` | Copy the selection to the other pane's directory |
| `d` / `Delete` | Delete the selection (asks first) |
| `v` | Mark / unmark; `Space` and `d` then act on all marked entries |
| `/` or `Ctrl-P` | Find files by name |
| `Ctrl-F` | Search file contents |
| `.` | Show / hide dotfiles |
| `s` | Sort by name, size or time |
| `r` | Refresh, and reconnect a dropped SSH pane |
| `Esc` | Close preview, cancel a running transfer, or clear marks |
| `?` / `F1` | Help |
| `q` / `Ctrl-C` | Quit (asks if a transfer is running) |

### Copying

- Copies go from the active pane into the other pane's current directory, in either direction.
- Copying a directory asks for confirmation first; plain files start straight away.
- If the target exists you choose: `o` overwrite (merge, for directories), `s` skip, `r` rename to `name (1).ext`, `a` overwrite all, `c` cancel.
- Files are written as `name.rcp-part` and renamed when complete, so a cancelled or failed copy never leaves a half-written file under the real name. Permissions and modification times are kept.
- Symlinks to files are copied as regular files; symlinks to directories are skipped (and reported) to avoid loops.
- Jobs started while one is running are queued.

### Deleting

`d` shows what will be removed (file count and size for directories) and only proceeds on `y`. Symlinks are removed, never followed.

### Preview

The preview replaces the other pane. Text files show their first 64 KiB, binary files show a hex dump, directories show their contents. Moving the cursor updates it.

### Search

Both searches start from the active pane's directory and work on local and remote panes. The window shows results on the left and a preview on the right.

**Find files** (`/`): fuzzy matching on the path, with fzf query syntax: `foo bar` (both), `'exact`, `^prefix`, `suffix$`, `!exclude`. Files ignored by `.gitignore` and dotfiles are skipped by default. On a remote host it runs `fd` or `find` on the server.

**Search text** (`Ctrl-F`): regex search with results grouped by file; the preview centres on the matching line. Runs 250 ms after you stop typing. On a remote host it runs `rg` (or `grep` if `rg` isn't installed) on the server. Stops at 10,000 matches.

| Key | Action |
|---|---|
| typing, `Backspace`, `Ctrl-W`, `Ctrl-U` | Edit the query |
| `↑` `↓` / `Ctrl-K` `Ctrl-J` / `Ctrl-P` `Ctrl-N` | Move through results |
| `PgUp` `PgDn` | Page |
| `Shift-↑` `Shift-↓` | Scroll the preview |
| `Enter` | Jump to the result in its pane |
| `Ctrl-Y` | Copy the result (or all marked results) to the other pane |
| `Tab` | Mark / unmark a result |
| `Alt-H` | Include dotfiles |
| `Alt-I` | Ignore `.gitignore` rules |
| `Alt-R` | Text search: regex ↔ fixed string |
| `Alt-C` | Text search: smart case → case-sensitive → ignore case |
| `Alt-W` | Text search: whole words only |
| `Alt-G` | Text search: edit a glob filter, e.g. `*.rs !target/**` |
| `Esc` | Close |

## Limitations

- Remote searches that fall back to `find`/`grep` don't apply `.gitignore`, and `grep` regex syntax can differ slightly from the local (Rust regex) syntax.
- No SSH keepalive; an idle connection dropped by the server needs `r`.
- Unix only (Linux, macOS).

## License

MIT. See [LICENSE](LICENSE).
