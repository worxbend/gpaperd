# gpaperd

A user daemon that rotates GNOME's wallpapers: one list of images for the
light style, another for the dark style. GNOME picks which one is on screen.
Sources are folders, single files and `http(s)` URLs. `gpaperctl` controls
the running daemon over D-Bus.

> **Status.** Nobody has yet checked slot reloading on a live GNOME 50 or 51
> session (the plan's Milestone 0). The gnome-shell source says replacing a
> slot file reloads the background. If your wallpaper does not change when
> gpaperd rotates, set `apply_mode = "uri"` in `[general]`. That is the
> fallback mode: each rotation writes the image's own URI to the keys.

## Why slots

gpaperd writes `picture-uri` and `picture-uri-dark` once. Each key points at
a file that gpaperd owns: `~/.local/share/gpaperd/slots/light` and `.../dark`.
A rotation replaces one slot file with `rename(2)`. gnome-shell's own file
monitor sees the change and reloads the background with its usual fade.

Why gpaperd does not write a new URI to the keys on every change:

- **Bounded GPU memory.** GNOME Shell 51 caches one texture per wallpaper URI
  and drops it only when that file changes. Two fixed URIs keep the cache at
  two entries. A new URI per rotation adds an entry for every image shown.
- **No extra reloads.** A write to any background key reloads the visible
  background, even a write to the hidden style's key. Rotating a slot writes
  no key.
- **No dconf churn.** Writing a key rewrites the whole dconf user database.

An XML slideshow is worse still: the shell wakes up every few seconds, keeps
every slide's texture, and the order is fixed when the file is generated.

## Features

- Light and dark styles each have their own sources, interval and order.
  Only the style on screen rotates, unless `rotate_hidden = true`.
- Random order uses a shuffle bag: every image once per round. Alphabetical
  order sorts file names naturally.
- inotify keeps each style's list of images current. Network and FUSE file
  systems are polled instead.
- Before an image is shown, gpaperd checks its magic bytes and decodes it
  with glycin, the sandboxed loader that gnome-shell uses.
- Remote images are cached and revalidated in the background. Rotation never
  waits for the network, and works offline from the cache.
- Wall-clock deadlines on a `timerfd`: correct across suspend and clock
  changes, and no polling.
- When something else changes the wallpaper, gpaperd pauses by default.
- The config file reloads on change, symlinked dotfiles included.
- Optional pause while the screen is locked (`pause_when_locked`).
- Ban an image you are tired of (`gpaperctl ban`), or copy one you like into
  a folder (`gpaperctl favorite`). Neither deletes or moves a file.
- Run your own command when the image changes (`on_change`), for example to
  recolour a terminal theme from the new wallpaper.
- A `Type=notify` systemd user unit that never restart-loops on a bad config.

## Install

The [user guide](docs/USER_GUIDE.md) walks through installing, starting,
configuring and removing gpaperd.

Install from the newest GitHub release (x86_64 or aarch64, glibc 2.39+;
no root needed). Save the script and read it, then run it:

```sh
curl -fsSLO https://raw.githubusercontent.com/worxbend/gpaperd/main/scripts/install.sh
less install.sh
sh install.sh
```

It checks the release archive against the release's `SHA256SUMS`, puts `gpaperd` and `gpaperctl` in `~/.local/bin`, installs the man pages
and the user unit, writes a starter config if you have none, and starts the
unit in a GNOME session. `sh install.sh --help` lists the options, including
`--uninstall`. `cargo binstall --git https://github.com/worxbend/gpaperd
gpaperd-daemon gpaperd-ctl` downloads the same archives.

### From source

Build dependencies: a Rust toolchain with edition 2024 (developed on
1.99.1), `pkg-config`, and the development files for GLib/GIO, libseccomp
and fontconfig. On Debian or Ubuntu:
`apt install pkg-config libglib2.0-dev libseccomp-dev libfontconfig-dev`.

At run time gpaperd needs a GNOME session. To decode images in their sandbox
it needs the glycin loaders and `bwrap`, which gnome-shell 51 itself uses.
Both are required: without them every image is refused as
`file is incomplete`, and the log warns `glycin cannot decode in its sandbox`.

From the workspace root:

```sh
cargo install --path crates/daemon --locked   # gpaperd
cargo install --path crates/ctl --locked      # gpaperctl
```

Both land in `~/.cargo/bin`, which is where the unit file looks.

Man pages (optional):

```sh
install -Dm644 dist/man/gpaperd.1      ~/.local/share/man/man1/gpaperd.1
install -Dm644 dist/man/gpaperctl.1    ~/.local/share/man/man1/gpaperctl.1
install -Dm644 dist/man/gpaperd.toml.5 ~/.local/share/man/man5/gpaperd.toml.5
```

## Set up

1. Write a config:

   ```sh
   install -Dm644 dist/config.example.toml ~/.config/gpaperd/config.toml
   $EDITOR ~/.config/gpaperd/config.toml
   gpaperd check-config
   ```

2. Install and start the user unit:

   ```sh
   install -Dm644 dist/gpaperd.service ~/.config/systemd/user/gpaperd.service
   systemctl --user daemon-reload
   systemctl --user enable --now gpaperd.service
   journalctl --user -u gpaperd -f
   ```

The unit is bound to `graphical-session.target`, so GNOME starts and stops it
with the session. Outside GNOME (`XDG_CURRENT_DESKTOP` without `GNOME`), its
`ExecCondition` skips the start. Exit codes 69 (not GNOME) and 78 (bad
config) stop systemd from restarting it. It reports `READY=1` after the first
wallpaper is applied, and `systemctl --user status gpaperd` shows a one-line
status: the image in each slot and the next change.

The unit file and the config can live in your dotfiles repository. A
symlinked config is followed, and relative paths in it resolve next to the
real file.

## Config reference

The file is `$XDG_CONFIG_HOME/gpaperd/config.toml`; `--config FILE`
overrides it. Every table and key is optional. An unknown key is an error.
Errors give a line and column. [`dist/config.example.toml`](dist/config.example.toml)
has every key with a comment. The full reference is `gpaperd.toml(5)`.

### `[general]`

| Key | Default | Meaning |
| --- | --- | --- |
| `interval` | `"15m"` | Time between changes, from `1m` to `7d` inclusive (`90s`, `15m`, `1h`, `1d`, `7d`). The deadline is saved, so a restart, reboot or suspend does not reset it |
| `order` | `"random"` | `"random"` (shuffle bag) or `"alphabetical"` (natural sort on the file name) |
| `picture_options` | unset | `wallpaper`, `centered`, `scaled`, `stretched`, `zoom` or `spanned`. Unset leaves GNOME's value, but `none` becomes `zoom` |
| `on_external_change` | `"pause"` | `"pause"`, `"reassert-next-tick"` or `"reassert-now"` (written back after 1 s; more than 3 changes a minute pauses) |
| `rotate_hidden` | `false` | Also rotate the style that is not on screen |
| `empty_style` | `"use-other-style"` | A style with no usable image shows the other style's image, or `"keep-current"` |
| `remote_refresh` | `"startup"` | Revalidate URLs once per start, every duration of at least `1h` (e.g. `"24h"`), or `"never"` |
| `link` | `true` | Fill slots by hardlink or reflink when possible. `false` always copies |
| `apply_mode` | `"slots"` | `"slots"` replaces two slot files. `"uri"` writes each image's URI to the keys |
| `max_pixels` | `100_000_000` | Skip images larger than this, width times height |
| `pause_when_locked` | `false` | Stop the timers while the screen is locked (`org.gnome.ScreenSaver`). A change that fell due happens at unlock |
| `favorites_dir` | unset | Where `gpaperctl favorite` copies the image on screen. A path, expanded as source paths are; created when missing. Unset, `favorite` fails |
| `on_change` | unset | A command, as a non-empty array of strings, that runs when the image changes. See [Hook command](#hook-command) |

### Hook command

`on_change = ["program", "arg", …]` runs with no shell. A bare program name
is looked up in `PATH`; a path must be absolute or start with `~/`. Only the
program gets `~` expanded; the arguments are passed as written. The daemon
does not wait for it: stdin is `/dev/null`, stdout and stderr go to the
journal. It runs when:

- a style shows another image: a rotation, `next`, `previous`, `set` or
  `ban`. That includes the style that is not on screen, with
  `GPAPERD_VISIBLE=0`.
- GNOME switches between light and dark.

It does not run for the same image published again, nor at start when the
slots still hold the images of the last run. The first start after
installing runs it for each style. In `apply_mode = "uri"` it runs once
GNOME reports the new key.

| Variable | Value |
| --- | --- |
| `GPAPERD_EVENT` | `changed` (a style shows another image) or `switched` (light/dark switch) |
| `GPAPERD_STYLE` | `light` or `dark`: the style that changed, or the one now on screen |
| `GPAPERD_ID` | The image's id, as `gpaperctl status` prints it |
| `GPAPERD_PATH` | The image's own file: the local file, or the cached download of a URL. Never the slot. Unset when the download is no longer known |
| `GPAPERD_VISIBLE` | `1` when that style is on screen, else `0` |

A run that starts while an earlier run for the same event is still running
starts anyway; the daemon logs at most one warning a minute about that. A
command that cannot start, or exits with a non-zero status, is logged with
its program and status.

### `[light]` and `[dark]`

| Key | Meaning |
| --- | --- |
| `interval`, `order` | Override `[general]` for this style |
| `sources` | List of sources (below). Empty is valid, see `empty_style` |

### Sources

| Form | Meaning |
| --- | --- |
| `"https://…"`, `"http://…"`, `{ url = "…" }` | A remote image, cached in `~/.cache/gpaperd/remote` |
| `"file:///…"` | A percent-decoded local path |
| `"~/Pictures/walls"` | A path. Directory or file is decided at scan time, so a missing path is not an error |
| `{ dir = "…" }` | A directory. Options: `recursive` (false, 16 levels max), `hidden` (false), `follow_symlinks` (true), `poll` (a period of at least `1s`) |
| `{ file = "…" }` | One file, whatever its name |

A table takes exactly one of `dir`, `file` or `url`.
`~`, `$VAR` and `${VAR}` are expanded. An undefined variable is an error.
Inside a directory only these extensions count: `jpg jpeg png webp jxl avif
heic heif tif tiff bmp gif qoi`. A file reached by two paths counts once.

### Checking a config in CI

`gpaperd check-config [--config FILE]` parses the file, resolves its paths
and scans every source. It does not touch GNOME, the session bus, the slots,
the state file or the cache. It prints one line per source: the image files
found by name, or for a URL whether a cached copy exists. It exits 0 when
the file is valid, 66 when the file is missing, and 78 when it is invalid
(the message gives line and column). A source that does not exist yet is
only reported, not an error.

```text
$ gpaperd check-config
config file /home/me/.config/gpaperd/config.toml: valid
light:
  /home/me/Pictures/Wallpapers/light: 42 images
  https://example.com/wallpapers/dawn.jpg: remote, cached
dark:
  /mnt/nas/wallpapers/dark: 0 images
    skipped /mnt/nas/wallpapers/dark: No such file or directory (os error 2)
```

## gpaperctl

```text
gpaperctl next     [--style light|dark|visible|both]   rotate now
gpaperctl previous [--style …]                         show the previous image again (20 per style)
gpaperctl set      [--style …] <image>                 show this file or listed URL now
gpaperctl ban      [--style …]                         never show the current image again; show the next
gpaperctl unban    <id>                                let a banned image be shown again
gpaperctl favorite [--style …]                         copy the current image into favorites_dir
gpaperctl pause    [--style …]                         stop a style's timer
gpaperctl resume   [--style …]                         end the pause; an overdue style rotates at once
gpaperctl reload                                       re-read the config; prints line and column on error
gpaperctl refresh                                      revalidate every remote image now
gpaperctl restore                                      put the original wallpaper back and pause both styles
gpaperctl status   [--json]                            what is shown, the next change, catalog sizes, pauses, bans
```

`--style` defaults to `visible`, the style on screen. `set` takes a local
file, resolved against the current directory, or a URL as the config lists
it. The image must be in the sources of each style meant, or nothing
changes. It restarts the interval, keeps a pause, and `previous` goes back
from it.

`ban` takes the image on screen out of rotation in both styles for good,
and rotates the style as `next` does. Bans are saved in the state file;
`status` lists them, and `unban <id>` takes one back, with the id as
`status` prints it (a URL with credentials or a query as the config lists
it). No `next`, `previous`, `set` or rotation shows a banned image. A style
left without another image follows `empty_style`; with `keep-current` the
banned image stays on screen and `ban` fails, but the ban holds.

`favorite` copies the image on screen into `favorites_dir`: a local file
under its own name, a remote image under the last path segment of its URL.
An existing file is never replaced; the copy gets a suffix such as
`day-1.jpg`. Without `favorites_dir` it fails.

The exit status is 0 on success, 1 when the daemon is not running
or reports an error, and 2 on a usage error.
D-Bus: name and interface `io.github.worxbend.Gpaperd1`, object
`/io/github/worxbend/Gpaperd1`. `set` calls `Set(style, id)`; `id` is an
absolute, canonical path or an `http(s)` URL, and one outside the style's
sources is `org.freedesktop.DBus.Error.InvalidArgs`. `ban`, `unban` and
`favorite` call `Ban(style)`, `Unban(id)` and `Favorite(style)`; an `Unban`
of an id that is not banned is `InvalidArgs` too.

## Files

| Path | Content |
| --- | --- |
| `~/.config/gpaperd/config.toml` | Config |
| `~/.local/share/gpaperd/slots/{light,dark}` | Slot files the keys point at. Under data, not cache, so a cache cleaner cannot blank the desktop |
| `~/.local/state/gpaperd/state.json` | Current images, shuffle bags, deadlines, pauses, history, banned images, your original keys |
| `~/.local/state/gpaperd/state.json.corrupt` | The last state file that could not be read. A newer one replaces it |
| `~/.cache/gpaperd/remote/` | Downloaded images |

Each path follows the matching `XDG_*_HOME` variable when it is set.

## Troubleshooting

- **The wallpaper never changes.** Run `gpaperctl status`. A style marked
  `paused: external change` means something else set the wallpaper; run
  `gpaperctl resume`. If status shows rotations but the screen does not
  change, slot reloading does not work on your GNOME release: set
  `apply_mode = "uri"`.
- **The unit is skipped.** `systemctl --user status gpaperd` says the
  condition failed: `XDG_CURRENT_DESKTOP` does not contain `GNOME`.
- **The unit failed with status 78.** The config is invalid. The journal and
  `gpaperd check-config` give the line and column. After a fix, run
  `systemctl --user restart gpaperd`. Edits while it runs need no restart.
- **The unit failed with status 69.** Not a GNOME session: the
  `org.gnome.desktop.background` schema is missing, or
  `GSETTINGS_BACKEND=memory` is set.
- **`paused: locked key`.** An administrator locked the background key on
  another value. gpaperd only rotates a style whose locked key already points
  at its slot. A lock added or lifted while gpaperd runs is followed at once.
- **Images listed as bad.** `gpaperctl status --json` gives each reason. HEIC
  and JPEG XL need the matching glycin loader.
- **A folder on NFS, SMB or FUSE updates late.** Those are polled every 5
  minutes. Set `poll = "1m"` on the `dir` source for a shorter period.
- **The log mentions `fs.inotify.max_user_watches`.** The inotify watch
  limit was hit, so that source is polled. Raise the sysctl and restart
  gpaperd to get instant updates again.
- **Undo everything.** `gpaperctl restore` writes back the keys you had
  before gpaperd first ran and pauses rotation. Then disable the unit.
  A key an administrator locked stays as it is: restore writes the others
  and fails, naming the locked keys.

Logs go to the journal at level info, with the fields `STYLE=`, `ITEM=`,
`METHOD=`, `REASON=` and `PATH=`:
`journalctl --user -u gpaperd STYLE=dark`.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

Tests never touch your real session. GSettings tests use compiled test
schemas with `GSETTINGS_BACKEND=keyfile`, and D-Bus tests run a private
`dbus-daemon`. Where glycin cannot decode (no loaders or no working
`bwrap`), the tests that need a decode print `skipped:` and pass; the
end-to-end scenarios that are not about decoding check magic bytes only,
in the tests, never in the daemon. `.github/workflows/ci.yml` runs the
same steps, plus
`.github/scripts/check-docs.sh`. That script checks that every key in the
example config is documented here and in `gpaperd.toml(5)`, and that every
command is in the man pages.

`scripts/soak.sh [SECONDS]` runs the real daemon for 20 minutes by default
in a sandbox (temp `HOME` and XDG dirs, keyfile backend, private bus) while
it adds, deletes and renames images, rewrites the config, deletes slots,
flips `color-scheme` and calls `gpaperctl`. It then checks that the daemon
is alive and exits 0, both slots hold an image, `state.json` is valid, RSS
stays bounded and no temp file is left. Build first with
`cargo build -p gpaperd-daemon -p gpaperd-ctl`. Clock steps, suspend and
network changes are not covered: they need privileges or a real session.

Workspace: `crates/core` (pure state machine), `crates/daemon` (adapters,
binary `gpaperd`), `crates/ctl` (binary `gpaperctl`), `dist/` (unit, example
config, man pages), `docs/` (user guide), `scripts/install.sh` (installer).

Releases: push a tag `vX.Y.Z` that matches the workspace version.
`.github/workflows/release.yml` builds x86_64 and aarch64 archives, writes
`SHA256SUMS` and publishes the GitHub release that `install.sh` and
cargo-binstall download from.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
