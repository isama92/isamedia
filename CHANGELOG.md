# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.12](https://github.com/isama92/isamedia/compare/v0.1.11...v0.1.12) - 2026-07-11

### Other

- bundle recommended mpv config ([#26](https://github.com/isama92/isamedia/pull/26))

## [0.1.11](https://github.com/isama92/isamedia/compare/v0.1.10...v0.1.11) - 2026-07-11

### Added

- reorder app tabs from settings ([#24](https://github.com/isama92/isamedia/pull/24))

## [0.1.10](https://github.com/isama92/isamedia/compare/v0.1.9...v0.1.10) - 2026-07-11

### Added

- hide unconfigured backend tabs and add backend removal ([#22](https://github.com/isama92/isamedia/pull/22))

## [0.1.9](https://github.com/isama92/isamedia/compare/v0.1.8...v0.1.9) - 2026-07-11

### Added

- stop playback with s from any tab ([#20](https://github.com/isama92/isamedia/pull/20))

### Other

- license change

## [0.1.8](https://github.com/isama92/isamedia/compare/v0.1.7...v0.1.8) - 2026-07-11

### Fixed

- surface swallowed mutation errors and assorted browse fixes ([#15](https://github.com/isama92/isamedia/pull/15))

## [0.1.7](https://github.com/isama92/isamedia/compare/v0.1.6...v0.1.7) - 2026-07-11

### Fixed

- recover from pause, revoked tokens, cancelled logins and partial browse loads ([#13](https://github.com/isama92/isamedia/pull/13))

## [0.1.6](https://github.com/isama92/isamedia/compare/v0.1.5...v0.1.6) - 2026-07-11

### Fixed

- player and rendering edge cases; move sort keybinding to v ([#11](https://github.com/isama92/isamedia/pull/11))

## [0.1.5](https://github.com/isama92/isamedia/compare/v0.1.4...v0.1.5) - 2026-07-10

### Fixed

- redact inbound mpv IPC logs and randomise the IPC endpoint name ([#8](https://github.com/isama92/isamedia/pull/8))

## [0.1.4](https://github.com/isama92/isamedia/compare/v0.1.3...v0.1.4) - 2026-07-10

### Fixed

- preserve browse state, atomic config writes, and robust player shutdown ([#7](https://github.com/isama92/isamedia/pull/7))

## [0.1.3](https://github.com/isama92/isamedia/compare/v0.1.2...v0.1.3) - 2026-07-10

### Other

- refresh README and --help for Radarr, Sonarr and language prefs ([#5](https://github.com/isama92/isamedia/pull/5))

## [0.1.2](https://github.com/isama92/isamedia/compare/v0.1.1...v0.1.2) - 2026-07-10

### Fixed

- playback lifecycle and *arr navigation/polling races ([#3](https://github.com/isama92/isamedia/pull/3))

## [0.1.1](https://github.com/isama92/isamedia/compare/v0.1.0...v0.1.1) - 2026-07-10

### Fixed

- add form commits the shown item, not one swapped in by a late lookup ([#1](https://github.com/isama92/isamedia/pull/1))

## [0.1.0] - 2026-07-10

### Added

- Initial release: terminal media client with a ratatui TUI, a Jellyfin backend, and mpv playback over IPC.
