# Agent Guidelines for isamedia

isamedia is a terminal media client. It renders a TUI with `ratatui`, talks to
media server REST APIs (currently Jellyfin) over `reqwest`/`tokio`, and drives
an external player (mpv) over IPC. These rules exist to keep that shape
correct as new apps/backends get added — skip anything below that clearly
doesn't apply to a change, but don't silently break the patterns.

## Stack

- `ratatui` for rendering, `tokio` (multi-thread runtime) for async work.
- `reqwest` (rustls, no native-tls) for HTTP; `serde`/`serde_json` for API
  payloads, `toml`/`serde_yaml` for config.
- `thiserror` for library-style error enums, `anyhow` only at outer
  boundaries (`main.rs`, `config.rs`, CLI).
- `tracing` (+ `tracing-subscriber`, `tracing-appender`) for all logging.
- `clap` for CLI parsing.

Don't reach for a new dependency to solve something the stack above already
covers, and don't add a dependency for a use case the project doesn't have
(no web framework, no dataframe library, no CPU-parallelism crate — this app
is I/O-bound on network calls, not compute-bound).

## Architecture conventions

- Elm-style split: a `Msg` enum carries results of spawned async work back
  into the event loop (see `apps/jellyfin/msg.rs`). Any async operation that
  a newer one can supersede (tab switch, re-fetch, cancel-and-reissue) must
  carry a generation counter (`auth_gen`, `fetch_gen`, `player_gen`) and be
  dropped on arrival if stale. Follow this pattern for every new async path,
  not just the existing ones.
- REST clients (`src/jellyfin`) stay pure HTTP with no `ratatui`/UI imports.
  UI-facing glue lives under `src/apps/*`.
- Async work spawned from the event loop reports back via `Msg`; it never
  blocks or is awaited on the render thread.

## REST client rules

- Every request path needs a timeout (client-level or per-request) — an
  unbounded call freezes the UI's ability to recover, since there's no
  request-independent way to cancel it from the render loop.
- Map HTTP statuses to specific `Error` variants (see `jellyfin::Error`)
  instead of a generic "request failed" — callers need to distinguish
  auth-expired (401) from server error from network failure.
- If you add retry/backoff, make it exponential with a capped attempt count.
  Don't add naive immediate retries against a self-hosted server.
- Any fetch triggered by user navigation must carry a generation counter and
  be dropped if superseded, per the existing `Msg` pattern above.

## TUI rules

- Never let a panic escape without restoring the terminal first (panic hook
  around `ratatui`'s restore). A raw-mode terminal left in a bad state on
  crash is worse than the crash itself.
- No blocking calls on the render/event thread. All I/O happens in spawned
  tokio tasks that report back via `Msg`.
- No `println!`/`eprintln!` anywhere reachable at runtime — stdout is the
  render surface. Use `tracing` macros exclusively.

## Error handling

- `thiserror` for fallible library code (`jellyfin::Error`, player errors).
  `anyhow` + `.context()` only at the outer boundary.
- No `.unwrap()`/`.expect()` on anything that can fail at runtime (network,
  filesystem, parsing) — a panic kills the whole session. `.expect()` is only
  for invariants already checked earlier in the same function.
- Propagate with `?`; don't swallow errors by logging and continuing unless
  the caller genuinely doesn't need to know.

## Code style

- `snake_case` / `PascalCase` / `SCREAMING_SNAKE_CASE`, 4-space indent,
  rustfmt's default line width.
- No wildcard imports except `use super::*;` in `#[cfg(test)]` modules.
- No emoji or emoji-lookalike unicode in UI strings or code, except in tests
  that specifically exercise multibyte rendering.
- Prefer borrowing over cloning; use iterators/combinators over manual loops
  where it reads at least as clearly. Don't force it where a loop is clearer.
- Doc comments on public items explain *why*, not what the signature already
  says (see `jellyfin::Client::send`, `apps/jellyfin/msg.rs` for the intended
  style).

## Testing

- Unit test pure logic (URL normalization, display formatting, config
  roundtrips) without network access.
- Tests that hit a real server are `#[ignore]`d and run manually (see
  `jellyfin::tests::demo_server_smoke`). Never let the default test run
  depend on network access.
- New `Error` variants get a test asserting the status-code-to-variant
  mapping, following the `AuthFailed`/`Unauthorized` examples.

## Security

- Credentials live only in the config file at `Config::default_path()`,
  written with `0600` permissions. Don't introduce a second place secrets
  could land (env vars, cache files, temp files).
- Never log tokens, passwords, or the `Authorization` header. Check
  `tracing` calls near auth/request code for accidental leakage.
- No secrets or real hostnames in code or tests. The demo-server test only
  ever talks to the public Jellyfin demo instance.

## Version control

- Clear, descriptive commit messages.
- No commented-out code, no debug `println!`/`dbg!` left behind.
- Don't leave a newly introduced `.unwrap()` in code that ships.

## Before finishing a change

- [ ] `cargo build` — no warnings
- [ ] `cargo clippy -- -D warnings`
- [ ] `cargo fmt --check`
- [ ] `cargo test`
- [ ] Any new async/fetch path that can be superseded carries a generation
      counter
- [ ] No blocking calls introduced on the render thread
- [ ] No secrets or tokens logged
