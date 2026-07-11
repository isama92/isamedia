# Recommended mpv config for isamedia

isamedia plays media in your own mpv, using your global mpv config (it does not
pass `--no-config`). This folder is a ready-to-use config that pairs well with
the app: a modern on-screen controller, a tidy keymap, and a contextual Skip
button.

Using it is optional. isamedia works with any mpv config, or none.

## Setup

Install mpv first: https://mpv.io/installation/

Then copy this folder into your mpv config directory. Back up any existing
config first, since this overwrites files with the same name.

Linux and macOS:

```sh
mkdir -p ~/.config/mpv
cp -r contrib/mpv/. ~/.config/mpv/
```

Windows (PowerShell), where mpv reads `%APPDATA%\mpv\`:

```powershell
Copy-Item -Recurse -Force contrib\mpv\* $env:APPDATA\mpv\
```

## What's included

- `mpv.conf` - disables mpv's built-in OSC so ModernZ can take over, hides the
  window title bar (ModernZ draws its own), quietens the centred OSD, and sets a
  screenshot directory and template.
- `input.conf` - the full keymap. Read this file to see and customise every
  binding: volume, seek, speed, subtitles, fullscreen, the ModernZ menu, and a
  deliberately trimmed set of defaults (many stock keys are set to `ignore`).
- `script-opts/modernz.conf` - the ModernZ on-screen controller settings (layout,
  icon theme, seek bar, window controls, which buttons show).
- `scripts/skip_button.lua` - a contextual "Skip Intro/Outro" button for
  chaptered files, first-party to isamedia (see below).
- `scripts/modernz.lua` and `fonts/modernz-icons.ttf` - ModernZ itself, vendored
  (see Third-party notices below).

## Third-party notices

### ModernZ

`scripts/modernz.lua` and `fonts/modernz-icons.ttf` are ModernZ v0.3.3, a modern
mpv on-screen controller.

- Upstream: https://github.com/Samillion/ModernZ
- Licence: GNU Lesser General Public License v2.1 (LGPL-2.1). The full text is in
  `LICENSE-ModernZ` in this folder.
- Lineage: ModernZ derives from mpv's official `osc.lua` via maoiscat's
  mpv-osc-modern and the cyl0/dexeonify ModernX forks.

The files are vendored unmodified. To update ModernZ, replace `scripts/modernz.lua`
and `fonts/modernz-icons.ttf` with a newer release from the upstream repository
and keep `LICENSE-ModernZ` in sync.

### skip_button.lua

`scripts/skip_button.lua` is first-party to isamedia by Stefano Borzoni, and is
covered by the isamedia project licence (GPL-3.0-only), not LGPL-2.1.
