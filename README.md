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
- Log in once; the token is stored and reused on later runs.

## Requirements

- mpv in `PATH`
- A Jellyfin server (10.9+)

## Build

```sh
cargo build --release
./target/release/isamedia
```

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

[jellyfin]
host = "https://jellyfin.example.com"   # http(s), base paths supported
username = "me"
password = ""              # optional; only used when the token expires
device = "hostname"
device_id = "..."          # generated
token = "..."              # managed automatically
user_id = "..."            # managed automatically
skip_segments = []         # e.g. ["Intro", "Outro", "Recap", "Preview", "Commercial"]
```

## Development

```sh
cargo test                       # unit tests
cargo test demo_server -- --ignored   # smoke test against demo.jellyfin.org
cargo clippy --all-targets
```

Architecture notes: one central event loop (tokio mpsc channel); all state
mutation is synchronous in the shell loop, every await lives in a spawned
task. Apps implement the `MediaApp` trait (`src/app.rs`); adding Sonarr later
means writing `src/apps/sonarr/` and registering it in
`src/apps/mod.rs::build_apps`. mpv is driven over its JSON IPC socket by a
supervisor task (`src/player/supervisor.rs`).

## Credits

Behaviour, keybindings and the Jellyfin/mpv integration are ported from
[jfsh](https://github.com/hacel/jfsh) by hacel (public domain / Unlicense).
