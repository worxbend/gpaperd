# gpaperd user guide

gpaperd changes your GNOME wallpaper on a timer. It keeps one set of images
for the light style and one for the dark style, and GNOME shows whichever
matches your appearance setting. `gpaperctl` controls the running daemon.

This guide covers installing, starting, configuring and removing gpaperd.
The full reference is in the man pages: `man gpaperd`, `man gpaperctl` and
`man 5 gpaperd.toml`.

## 1. Requirements

- Linux, x86_64 or aarch64, with a GNOME session (GNOME 42 or newer for
  separate light and dark images).
- systemd as the user session manager. GNOME on almost every distribution
  uses it.
- GLib, libseccomp and fontconfig. A GNOME desktop already has them.
- Recommended: `glycin-loaders` and `bubblewrap` (`bwrap`). With them,
  gpaperd checks each image with the same sandboxed decoders gnome-shell
  uses. Without them it checks only the file's magic bytes.

The release binaries are built on Ubuntu 24.04 and need glibc 2.39 or newer.
On an older system, build from source (section 2.3).

## 2. Install

### 2.1 Install script (recommended)

```sh
curl -fsSL https://raw.githubusercontent.com/worxbend/gpaperd/main/scripts/install.sh | sh
```

The script:

1. downloads the newest release for your CPU from GitHub and checks it
   against the release's `SHA256SUMS`;
2. installs `gpaperd` and `gpaperctl` into `~/.local/bin`, the man pages into
   `~/.local/share/man`, and the user unit into
   `~/.config/systemd/user/gpaperd.service`;
3. writes a starter config to `~/.config/gpaperd/config.toml` if you have
   none, creates `~/Pictures/Wallpapers/light` and `~/Pictures/Wallpapers/dark`,
   and puts the fully commented `config.example.toml` next to it;
4. warns about missing libraries, loaders or a `~/.local/bin` missing from
   your `PATH`;
5. in a GNOME session, enables and starts the unit. Outside GNOME it only
   installs the unit.

It needs no root and touches nothing outside your home directory.

Options, passed after `sh -s --`:

```sh
curl -fsSL .../install.sh | sh -s -- --version v0.1.1   # a given release
curl -fsSL .../install.sh | sh -s -- --no-start         # install, do not start
curl -fsSL .../install.sh | sh -s -- --prefix ~/apps    # binaries in ~/apps/bin
```

To read the script before running it:

```sh
curl -fsSLO https://raw.githubusercontent.com/worxbend/gpaperd/main/scripts/install.sh
less install.sh
sh install.sh --help
```

### 2.2 cargo binstall

If you use [cargo-binstall](https://github.com/cargo-bins/cargo-binstall),
it downloads the same release archives:

```sh
cargo binstall --git https://github.com/worxbend/gpaperd gpaperd-daemon gpaperd-ctl
```

The binaries land in `~/.cargo/bin`. binstall installs no unit, man pages or
config: follow section 3 with the files from the repository's `dist/`
folder.

### 2.3 From source

Install a Rust toolchain (edition 2024) and the development packages. On
Debian or Ubuntu:

```sh
sudo apt install pkg-config libglib2.0-dev libseccomp-dev libfontconfig-dev
```

Then:

```sh
git clone https://github.com/worxbend/gpaperd
cd gpaperd
cargo install --path crates/daemon --locked   # gpaperd
cargo install --path crates/ctl --locked      # gpaperctl
```

The binaries land in `~/.cargo/bin`, which is where `dist/gpaperd.service`
looks. Continue with section 3.

## 3. Start

The install script does this for you in a GNOME session. By hand:

```sh
install -Dm644 dist/gpaperd.service ~/.config/systemd/user/gpaperd.service
systemctl --user daemon-reload
systemctl --user enable --now gpaperd.service
```

`enable` makes it start with every GNOME login; `--now` starts it at once.
The unit belongs to the graphical session, so it stops when you log out.
Outside GNOME it skips itself.

Check that it runs:

```sh
gpaperctl status
systemctl --user status gpaperd
journalctl --user -u gpaperd -f
```

The first time it runs, gpaperd saves your current wallpaper settings, then
points GNOME's background keys at two files it manages in
`~/.local/share/gpaperd/slots/`. `gpaperctl restore` puts your old settings
back at any time.

## 4. Configure

The config file is `~/.config/gpaperd/config.toml`. gpaperd applies an edit
while it runs; you do not need to restart it. If an edit has an error, the
running config stays and the journal shows the line and column. To check a
file yourself:

```sh
gpaperd check-config
```

It prints how many images each source has and exits non-zero on errors, so
it also works in a dotfiles CI job.

### 4.1 A minimal config

```toml
[general]
interval = "15m"        # how often the wallpaper changes: 1m to 1h
order = "random"        # or "alphabetical"

[light]
sources = ["~/Pictures/Wallpapers/light"]

[dark]
sources = ["~/Pictures/Wallpapers/dark"]
```

Drop images into those folders. New, removed and renamed files are picked up
at once. Supported formats: JPEG, PNG, WebP, JPEG XL, AVIF, HEIC/HEIF, TIFF,
BMP, GIF and QOI (JPEG XL and HEIC need the matching glycin loader).

### 4.2 Sources

Each style takes a list of sources. Mix them freely:

```toml
[light]
sources = [
  "~/Pictures/Wallpapers/light",                         # a folder (or a file)
  { dir = "~/Pictures/Wallpapers/shared", recursive = true },
  { file = "~/Pictures/hero.jxl" },
  "https://example.com/wallpapers/dawn.jpg",             # downloaded and cached
]
```

- `~`, `$VAR` and `${VAR}` are expanded. Relative paths are relative to the
  config file, so a config kept in a dotfiles repository works.
- A folder that does not exist yet is fine; gpaperd waits for it.
- Folders on NFS, SMB or FUSE are polled every 5 minutes. Set
  `{ dir = "...", poll = "1m" }` for another period.
- Downloaded images are kept in `~/.cache/gpaperd/remote` and shown even when
  you are offline.
- The same folder can appear in both styles.

### 4.3 Common settings

All keys go under `[general]`. `interval` and `order` can also be set per
style under `[light]` or `[dark]`.

| Key | Default | What it does |
| --- | --- | --- |
| `interval` | `"15m"` | Time between changes, `1m` to `1h`. Counts only time the style is on screen |
| `order` | `"random"` | `random` shows every image once per round; `alphabetical` follows file names |
| `picture_options` | not set | `zoom`, `scaled`, `centered`, `stretched`, `wallpaper` or `spanned`; unset leaves GNOME's choice |
| `rotate_hidden` | `false` | Also rotate the style that is not on screen |
| `empty_style` | `"use-other-style"` | A style without images shows the other style's image; `keep-current` leaves it |
| `on_external_change` | `"pause"` | When you pick a wallpaper in Settings: `pause` gpaperd, or put its own back (`reassert-next-tick`, `reassert-now`) |
| `pause_when_locked` | `false` | Stop the timers while the screen is locked |
| `remote_refresh` | `"startup"` | Check downloaded images for a new version: `startup`, a period of 1 h or more, or `never` |
| `link` | `true` | Hardlink images into the slot when possible; `false` always copies |
| `apply_mode` | `"slots"` | `uri` writes each image's own path to GNOME instead of using slot files |
| `max_pixels` | `100_000_000` | Skip images larger than this (width × height) |

`~/.config/gpaperd/config.example.toml` shows every key with comments.

### 4.4 Examples

Change every 5 minutes in daylight, every 30 at night, both in file-name
order:

```toml
[general]
order = "alphabetical"

[light]
interval = "5m"
sources = ["~/Pictures/Wallpapers/day"]

[dark]
interval = "30m"
sources = ["~/Pictures/Wallpapers/night"]
```

One folder for both styles, and keep gpaperd's choice even when another
tool changes the wallpaper:

```toml
[general]
on_external_change = "reassert-next-tick"

[light]
sources = ["~/Pictures/Wallpapers"]

[dark]
sources = ["~/Pictures/Wallpapers"]
```

## 5. Everyday use

```sh
gpaperctl next                 # next image now, for the style on screen
gpaperctl next --style both    # both styles
gpaperctl previous             # back to the image before (20 kept per style)
gpaperctl pause                # stop changing; resume continues
gpaperctl resume
gpaperctl status               # what is shown, when it changes next
gpaperctl status --json
gpaperctl reload               # apply the config now and show any error
gpaperctl refresh              # check downloaded images for new versions
gpaperctl restore              # put your original wallpaper back and pause
```

`--style` takes `light`, `dark`, `visible` (the default) or `both`.

If you choose a wallpaper in GNOME Settings, gpaperd pauses that style
(with the default `on_external_change = "pause"`). `gpaperctl resume` takes
over again.

## 6. Update

Run the install script again. It replaces the binaries, keeps your config,
and restarts gpaperd if it was running:

```sh
curl -fsSL https://raw.githubusercontent.com/worxbend/gpaperd/main/scripts/install.sh | sh
```

With binstall or from source, reinstall the same way, then
`systemctl --user restart gpaperd`.

## 7. Uninstall

```sh
curl -fsSL https://raw.githubusercontent.com/worxbend/gpaperd/main/scripts/install.sh | sh -s -- --uninstall
```

This puts your original wallpaper back (`gpaperctl restore`), stops and
disables the unit, and removes the binaries, man pages and unit. Your config
and state stay; add `--purge` to remove them too, along with the slot files
and the download cache.

By hand:

```sh
gpaperctl restore
systemctl --user disable --now gpaperd.service
rm ~/.config/systemd/user/gpaperd.service ~/.local/bin/gpaperd ~/.local/bin/gpaperctl
systemctl --user daemon-reload
```

## 8. Troubleshooting

| Symptom | What to do |
| --- | --- |
| `gpaperctl` says gpaperd is not running | `systemctl --user status gpaperd`, then `journalctl --user -u gpaperd` |
| The unit is skipped ("condition failed") | Not a GNOME session: `XDG_CURRENT_DESKTOP` does not contain `GNOME` |
| The unit failed with status 78 | The config has an error: `gpaperd check-config` shows line and column. Fix it, then `systemctl --user restart gpaperd` |
| The unit failed with status 69 | GNOME's background settings are missing: not a GNOME session |
| Status says `paused: external change` | Something else set the wallpaper. `gpaperctl resume` |
| Status shows changes but the screen does not change | Your GNOME release does not reload slot files. Set `apply_mode = "uri"` |
| Some images are never shown | `gpaperctl status --json` lists bad images with the reason. HEIC and JPEG XL need glycin loaders |
| A network folder updates late | It is polled; set `poll = "1m"` on that source |
| `gpaperctl: command not found` | Add `~/.local/bin` to your `PATH` |

More cases are in the README's Troubleshooting section and in `man gpaperd`.

## Known limits

gpaperd 0.1 has not yet been tested on a live GNOME 50 or 51 session. If
the wallpaper does not change while `gpaperctl status` shows new images, set
`apply_mode = "uri"` and please open an issue at
<https://github.com/worxbend/gpaperd/issues>.
