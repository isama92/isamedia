## What changed and why

<!-- Briefly describe the change and the reason for it. -->

## Release note

The **PR title** must be a Conventional Commit: on squash-merge it becomes the
commit message that release-plz reads to compute the next version.

- `feat: ...` bumps the minor version
- `fix: ...` bumps the patch version
- `feat!: ...` or a `BREAKING CHANGE:` footer bumps the major version
- `chore: `, `docs: `, `refactor: `, `test: `, etc. do not cut a release on their own

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo build`, `cargo test` pass
- [ ] Any new async/fetch path that can be superseded carries a generation counter
- [ ] No blocking calls introduced on the render thread (bar the sanctioned `Config::save`)
- [ ] No secrets or tokens logged, including inside URLs handed to mpv
