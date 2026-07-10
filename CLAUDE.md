# Agent Guidelines for isamedia

isamedia is a terminal media client. It renders a TUI with `ratatui`, talks to
media server REST APIs (currently Jellyfin) over `reqwest`/`tokio`, and drives
an external player (mpv) over IPC. These rules exist to keep that shape
correct as new apps/backends get added — skip anything below that clearly
doesn't apply to a change, but don't silently break the patterns.

## Stack

- `ratatui` for rendering, `tokio` (multi-thread runtime) for async work.
- `reqwest` for HTTP (rustls on unix, native-tls/schannel on Windows);
  `serde`/`serde_json` for API payloads, `toml` for config.
- `keyring` for secrets (Secret Service on Linux, Credential Manager on
  Windows); see the Security rules.
- `thiserror` for library-style error enums, `anyhow` only at outer
  boundaries (`main.rs`, `config.rs`, CLI).
- `tracing` (+ `tracing-subscriber`, `tracing-appender`) for all logging.
- `clap` for CLI parsing, `directories` for the config path, `uuid` for the
  generated device id, `gethostname` for the device name.

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

## Apps and the shell

- Each tab is a `MediaApp` (see `app.rs`). Apps own their entire keymap and
  state; the shell only handles quit, app switching, and frame chrome (tab
  bar plus status bar). Route nothing app-specific through the shell.
- Every app defines its own private `Msg` enum and sends it through
  `AppSender`. The shell carries it as a type-erased `Box<dyn Any + Send>`;
  the owning app downcasts in `on_event` and ignores foreign payloads. The
  shell never needs to know an app's message types.
- `status_line` may surface cross-tab state (e.g. a now-playing bar visible
  from another tab). `on_quit` returns `true` to request a short shutdown
  grace period for flush work such as the mpv quit and final playback report.
- Adding an app (Sonarr, Radarr) means writing its module and swapping its
  `ComingSoonApp` entry in `apps::build_apps`; the shell needs no changes.
  Implement the full trait, including the generation-counter pattern on every
  async path.

## REST client rules

- Every request path needs a timeout (client-level or per-request) — an
  unbounded call freezes the UI's ability to recover, since there's no
  request-independent way to cancel it from the render loop.
- Map HTTP statuses to specific `Error` variants (see `jellyfin::Error`)
  instead of a generic "request failed" — callers need to distinguish
  auth-expired (401) from server error from network failure.
- If you add retry/backoff, make it exponential with a capped attempt count.
  Don't add naive immediate retries against a self-hosted server.
- Any fetch or mutation triggered by user action must carry a generation
  counter and be dropped if superseded (for example a watched-toggle result
  that lands after the user has switched tabs), per the `Msg` pattern above.

## Player and external processes

- The player runs mpv as a child process over JSON IPC. Playback state flows
  back to the UI only as `PlayerEvent`s; never block the render loop to query
  the player. A supervisor task owns the process and its socket.
- Any spawned external process sets `kill_on_drop`, removes its IPC socket on
  every exit path, and connects with a bounded poll (capped attempts, not an
  unbounded wait) so a player that never starts cannot wedge the app.
- A replaced player is told apart by `player_gen`; drop events whose
  generation is stale, exactly like fetches.
- Prefer authenticating mpv's stream requests with a header over a token in
  the URL. If a credential must ride in the URL (e.g. a Jellyfin `api_key`),
  redact it before it can reach a log; the mpv IPC debug log prints full
  commands, URLs included. See the Security rules.

## TUI rules

- Never let a panic escape without restoring the terminal first (panic hook
  around `ratatui`'s restore). A raw-mode terminal left in a bad state on
  crash is worse than the crash itself.
- No blocking calls on the render/event thread. All I/O happens in spawned
  tokio tasks that report back via `Msg`. The one sanctioned exception is the
  small synchronous `Config::save` on app switch and after auth; keep such
  writes tiny and push anything heavier onto a spawned task.
- No `println!`/`eprintln!` anywhere reachable at runtime — stdout is the
  render surface. Use `tracing` macros exclusively.

## Error handling

- `thiserror` for fallible library code (`jellyfin::Error`, player errors).
  `anyhow` + `.context()` only at the outer boundary.
- No `.unwrap()`/`.expect()` on anything that can fail at runtime (network,
  filesystem, parsing) — a panic kills the whole session. `.expect()` is only
  for invariants already checked earlier in the same function.
- The one accepted unwrap on a runtime failure is `.lock().unwrap()` on the
  config mutex: a poisoned lock means another thread already panicked and the
  session is unrecoverable, and the panic hook still restores the terminal.
  Do not extend this to network, filesystem, or parse results.
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
- The status-to-`Error`-variant mapping lives in `Client::send`, so today it
  is only exercised by the ignored `demo_server_smoke` test. If you add
  variants and want offline coverage, stand up an HTTP mock instead of
  reaching for the network; never make the default test run depend on it.

## Security

- Secrets (session token, optional password) live only in the OS keyring via
  `src/secrets.rs` (service `isamedia`) — never in the config file, env vars,
  cache files, or temp files. The config file at `Config::default_path()`
  holds only non-secrets (host, username, device_id, user_id) and is still
  written `0600`; don't widen that, and don't add secret fields back to it.
- Keyring calls are blocking (a D-Bus round trip on Linux): always run them
  through `tokio::task::spawn_blocking`, never on the render thread.
- Never log tokens, passwords, or the `Authorization` header, including a
  credential embedded in a URL (a Jellyfin `api_key` on a stream URL would
  otherwise land in the mpv IPC debug log). Check `tracing` calls near auth,
  request, and player code for accidental leakage.
- No secrets or real hostnames in code or tests. The demo-server test only
  ever talks to the public Jellyfin demo instance.

## Version control

- The repo is a bare clone with per-branch worktrees living inside it:

  ```
  isamedia/
    .bare/                 # bare git dir
    .git                   # file containing "gitdir: ./.bare"
    main/                  # worktree of main
    feature/<branch-name>/ # worktree of a feature branch
  ```

  Run git from the `isamedia/` root or from inside a worktree.
- Never commit directly to `main`. Start every new feature on its own branch
  named `feature/<branch-name>`, checked out in its own worktree so several
  features progress at once without recloning:

  ```sh
  git worktree add feature/<branch-name> -b feature/<branch-name>
  ```

  Use matching prefixes for non-feature work: `fix/`, `docs/`, `chore/`,
  `refactor/`.
- Each worktree builds into its own `target/` by default, so a fresh feature
  recompiles from scratch and uses more disk. Sharing one build directory
  across worktrees (via `CARGO_TARGET_DIR`, or `build.target-dir` in a
  user-level `~/.cargo/config.toml`) avoids that, at the cost of serialising
  concurrent builds since cargo locks the target dir. Choose per your
  disk-versus-parallel-build trade-off.
- Keep branches short-lived: rebase on `main` regularly and open one PR per
  feature. Run the full "Before finishing a change" checklist and confirm it
  is green before pushing.
- After a branch merges, clean up: `git worktree remove feature/<branch-name>`
  and delete the branch so stale worktrees and merged branches don't
  accumulate.
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
- [ ] No blocking calls introduced on the render thread (bar the sanctioned
      `Config::save`)
- [ ] No secrets or tokens logged, including inside URLs handed to mpv
- [ ] Any spawned external process cleans up (`kill_on_drop` plus socket
      removal) on every exit path
- [ ] A new app implements the full `MediaApp` contract
