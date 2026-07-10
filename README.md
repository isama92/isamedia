# isamedia

A terminal client for your media stack. Browse your Jellyfin library and play
it in [mpv](https://mpv.io), without leaving the terminal. Sonarr and Radarr
support is planned; they already have (disabled) tabs in the UI.

isamedia is a Rust rewrite and extension of
[jfsh](https://github.com/hacel/jfsh), keeping feature parity while adding a
multi-app shell and non-blocking playback (the UI stays interactive while mpv
runs).

## Features

- **Jellyfin**: Resume / Next Up / Recently Added / Search tabs, series
  drill-down, client-side filtering, watched toggling
- **Playback in mpv** with your own mpv config: direct play, resume from last
  position, whole-series playlists for episodes, automatic progress reporting
  back to Jellyfin, media segment skipping (intro/outro), external subtitles
- **Non-blocking**: keep browsing while something plays; a status bar shows
  the current item and position, `s` stops playback
- **Multi-app shell**: switch apps with `ctrl+←/→` or `ctrl+1..3`
  (Sonarr/Radarr are placeholders for now)
- Log in once; the token is stored in the OS keyring (Secret Service on
  Linux, Credential Manager on Windows) and reused on later runs.

## Requirements

- mpv in `PATH`
- A Jellyfin server (10.9+)
- On Linux: a Secret Service keyring (GNOME Keyring or KWallet — present on
  any normal desktop) for storing the login token

## Build

```sh
cargo build --release
./target/release/isamedia
```

To install it as a command on your PATH (into `~/.cargo/bin`):

```sh
cargo install --locked --path .
isamedia
```

`--locked` reuses the exact dependency versions from `Cargo.lock`. The binary
is a snapshot, so re-run the command to pick up later source changes; remove
it with `cargo uninstall isamedia`.

Linux and Windows are supported (macOS should work too, but is untested).

## Usage

```
isamedia [OPTIONS]
  -c, --config <FILE>  config file path
  -d, --debug <FILE>   debug log file path (enables debug logging)
  -V, --version        print version
```

### Keys

| Key | Action |
| --- | --- |
| `ctrl+←/→`, `ctrl+1..3` | switch app |
| `ctrl+t` | choose colour theme |
| `←/h` `→/l` | previous / next tab |
| `↑/k` `↓/j`, `pgup/b/u`, `pgdn/f/d`, `g`, `G` | move around lists |
| `enter` / `space` | play item / open series |
| `esc` / `backspace` | back / clear |
| `/` | search (Search tab) or filter the current list |
| `w` | toggle watched |
| `r` | refresh |
| `s` | stop playback |
| `?` | full help |
| `q`, `ctrl+c` | quit |

## Configuration

`~/.config/isamedia/config.toml` on Linux (`%APPDATA%\isamedia\` on Windows),
created on first run. The login screen collects host and credentials the
first time; after that the stored token is used.

```toml
last_app = "jellyfin"
theme = "latte"            # "latte" or "solarized-light"

[jellyfin]
host = "https://jellyfin.example.com"   # http(s), base paths supported
username = "me"
device = "hostname"
device_id = "..."          # generated
user_id = "..."            # managed automatically
skip_segments = []         # e.g. ["Intro", "Outro", "Recap", "Preview", "Commercial"]
```

The config file holds no secrets. The session token — and the password, if
you enter one at login — are stored in the OS keyring under the `isamedia`
service (`jellyfin-token` / `jellyfin-password`). The password is optional
and only used to re-authenticate automatically when the token expires; leave
the field empty at login to not store one.

### Themes

Two light themes ship: Catppuccin Latte (default) and Solarized Light. Press
`ctrl+t` to open the picker, choose with the arrow keys and `enter`, and the
choice is saved to `theme` in the config. Both are light themes that only set
foreground colours and leave your terminal's own background alone, so they look
best on a light terminal.

## Development

```sh
cargo test                       # unit tests
cargo test demo_server -- --ignored   # smoke test against demo.jellyfin.org
cargo clippy --all-targets
```

### Git hooks

A committed `pre-commit` hook rejects a commit whose staged Rust changes are
not `rustfmt`-clean or trip clippy. Enable it once per clone:

```sh
git config core.hooksPath .githooks
```

When a `.rs` file is staged the hook runs `cargo fmt --all --check` first,
then `cargo clippy --all-targets -- -D warnings`. Bypass it in an emergency
with `git commit --no-verify`.

Architecture notes: one central event loop (tokio mpsc channel); all state
mutation is synchronous in the shell loop, every await lives in a spawned
task. Apps implement the `MediaApp` trait (`src/app.rs`); adding Sonarr later
means writing `src/apps/sonarr/` and registering it in
`src/apps/mod.rs::build_apps`. mpv is driven over its JSON IPC socket by a
supervisor task (`src/player/supervisor.rs`).

## Credits

Behaviour, keybindings and the Jellyfin/mpv integration are ported from
[jfsh](https://github.com/hacel/jfsh) by hacel (public domain / Unlicense).
