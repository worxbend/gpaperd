# gpaperd — GNOME light/dark wallpaper rotation daemon (Rust): research & implementation plan

Nov 14, 2026 · @Oleksandr

## Gist

**Decision:** a Rust user daemon writes `picture-uri` and `picture-uri-dark` once, pointing them at two files it owns (`slots/light`, `slots/dark`). It rotates wallpapers by atomically replacing each slot's content. There is no XML slideshow and no per-tick dconf write.

Why this beats the alternatives:

- **Bounded GPU memory.** GNOME Shell 51 keeps one texture per wallpaper URI and evicts it only when that file changes on disk. Two fixed URIs keep that cache at about two entries. A new URI per rotation grows it by the number of distinct images.
- **Native reload path.** Replacing a slot with `rename(2)` fires gnome-shell's own file monitor. The shell purges the texture and reloads with its normal 1 s fade.
- **No spurious reloads.** Writing *any* `org.gnome.desktop.background` key reloads the visible background, including a write to the hidden style's key. Slots never write keys during rotation.
- **XML is worse here.** With 1–15 min slides, a slideshow wakes gnome-shell every 1–14 s, holds every slide's texture after one cycle, and fixes the order when the file is generated.

**Shape:** one tokio core (a pure state machine) plus a GLib thread for GSettings, inotify watchers, a `timerfd` wall-clock timer and a `reqwest` fetcher. It runs as a `Type=notify` systemd user unit bound to `graphical-session.target`. Config is `~/.config/gpaperd/config.toml` with hot reload. `gpaperctl` controls it over D-Bus.

**Status:** a spike of config parsing, ordering, slot publishing, the wall-clock timer and the GSettings adapter compiles, and 10 unit tests pass. Nothing has run against a live GNOME session yet; Milestone 0 does that. `gpaperd` is a working name.

## What shell-wallpaper-rotator does

The extension is small (727 lines of JS, 4 commits, all on 2026-10-18) and gets the core idea right: it never reads `color-scheme`, it just fills both keys and lets GNOME pick. Everything else is simple, and that simplicity is where its gaps are.

| Area | Extension behaviour (file) | gpaperd |
| --- | --- | --- |
| Process model | Runs inside gnome-shell (`extension.js`) | Separate daemon; a crash cannot take the shell down |
| Style handling | Writes both keys; GNOME shows one (`wallpaperSetter.js`) | Same principle, but via slot files |
| Key writes | Two separate `set_string` calls per change; forces `picture-options` to `zoom` if `none` | Keys written once in one transaction; same `none` fix |
| Scanning | Full rescan of every folder on every change; `image/*` content type, skips hidden, depth 5 (`imageScanner.js`) | Indexed catalog updated by inotify; decodability probe |
| Random order | Uniform pick excluding the current image, so repeats are common (`rotator.js` `_pick`) | Shuffle bag: every image once per round |
| Alphabetical order | `indexOf(current) + 1`; an unknown current URI restarts at the first image | Sorted cursor resumes after a deleted or replaced item |
| Sorting | `localeCompare` on percent-encoded URIs | Natural, case-insensitive sort on file names |
| Empty style | Uses the other style's pick for both | Same, configurable |
| Timer | Sleeps ≤ 60 s, then re-checks the wall clock; clock going back starts a fresh interval | `timerfd` on `CLOCK_REALTIME` with cancel-on-set: no polling |
| Persistence | `last-change` timestamp in its own GSettings schema | State file with cursor, bag and timestamps |
| External changes | Ignored; the next tick overwrites the user's choice | Detected; policy decides (pause by default) |
| Remote images, file watching, validation | None | All three |

Worth keeping: wall-clock `last-change` scheduling, the empty-style fallback, serialized changes (its `_busy`/`_pending` flags), and cancellable scans.

## How GNOME Shell loads the background

These facts were read from source at gnome-shell 51.1, mutter 51.1, GLib 2.91, dconf and gnome-desktop HEAD (Nov 2026). They decide the design. Line numbers refer to `js/ui/background.js` unless another file is named.

| # | Fact | Where | Consequence for gpaperd |
| --- | --- | --- | --- |
| 1 | Dark key is used only when `color-scheme` is `prefer-dark`; `default` and `prefer-light` mean light | 749–757 | Use exactly this predicate |
| 2 | Any change to any `org.gnome.desktop.background` key, or to `color-scheme`, rebuilds the background; changes are coalesced in one idle, then the old actor fades out over 1000 ms | 119, 429–471, 886–903 | Writing the hidden key still reloads the visible one: avoid key writes during rotation |
| 3 | `picture-options = none` loads no file at all | 750 | Fix `none` to `zoom` on startup |
| 4 | GNOME 51 caches textures in a `Map` keyed by URI; only `purge(file)` removes an entry | 237–255, 375–377 | Unique URI per rotation = one resident texture per distinct image |
| 5 | `purge` runs only inside a live `Background`'s file-watch handler | 515–528 | Replacing a file nobody is displaying leaves a **stale** texture under that URI |
| 6 | The cache landed in commit 8b61c2a (2026-03-16): in 51.0, not in 50.6. GNOME ≤ 50 used mutter's cache, which drops an entry when its image is freed | mutter 49.0 `meta-background-image.c` 386–391 | Slot mode is needed on 51+, harmless on ≤ 50 |
| 7 | Each displayed file gets a `GFileMonitor` that is never removed; it reacts to `CHANGES_DONE_HINT`/`DELETED`, not `CHANGED`/`CREATED` | 150–166 | Write slots by `rename(2)`, never in place |
| 8 | GLib reports a rename onto the watched name as `CREATED` + `CHANGES_DONE_HINT` | GLib `glocalfilemonitor.c` 325–331, 446–451 | Rename-over triggers purge + reload |
| 9 | If the shown file is deleted, the reload fails and only the solid colour remains | 501–513, 642–646 | Slots live in a data dir, are watched, and are recreated at once |
| 10 | Images are decoded by glycin from a stream, so bytes decide the format; only `application/xml` content means a slideshow | 257–278, 662–664 | Slot files need no extension (verify in M0) |
| 11 | A slideshow wakes the shell every max(1 s, slide length ÷ 63.75); mutter skips the repaint if nothing changed | 125–126, 586–612; mutter `meta-background.c` 928–933 | XML costs JS wakeups, not repaints |
| 12 | Lock screen uses the same schema, blurred | `unlockDialog.js` 792 | One set of slots covers both |
| 13 | dconf rewrites the whole user database on each commit and drops writes equal to the current value | dconf `dconf-writer.c` 133, 174 | Re-asserting keys is free; per-tick writes are not |
| 14 | The Wallpaper portal sets both keys to the same URI | xdg-desktop-portal-gnome `wallpaper.c` 81–83 | Portal is unusable for per-style images |

Row 4 is a reading of the code, not a measurement. Milestone 0 measures gnome-shell memory over 50 rotations to confirm it.

## Approach: XML vs key writes vs slot files

Use slot files. Do not generate or update XML slideshows: they wake the shell constantly, fix the order at generation time, and pin every slide's texture. Per-tick key writes work but cost a dconf commit, an extra reload and unbounded texture growth on GNOME 51.

| Criterion | XML slideshow per style | New URI per tick (extension style) | Slot files (chosen) |
| --- | --- | --- | --- |
| Shell wakeups between changes | Every max(1 s, slide ÷ 63.75): \~1 s at 1 min, \~14 s at 15 min | None | None |
| dconf commits per rotation | 0 (one per regeneration) | 1, whole user DB rewritten | 0 |
| Visible reloads per rotation | 1; crossfade steps ≥ 1 s, so a 2 s transition is \~2 frames | 1, plus 1 more when the hidden key is written | 1 |
| GNOME 51 texture cache | Every slide after one cycle | Every distinct image shown | 2 entries, one per slot URI |
| Random order | Only by regenerating the file | Yes | Yes |
| Image deleted mid-run | Blank until regenerated | Daemon re-picks | Daemon re-picks; hardlinked slot still holds the data |
| User file names with `#`, `%`, spaces | XML escaping | URI percent-encoding | Never exposed: slot names are fixed |
| Behaviour when the daemon is stopped | Keeps cycling | Last image stays | Last image stays |
| Daemon complexity | Low | Low | Medium: hidden-style refresh, link/copy fallback |

Fallback: keep `apply_mode = "uri"` (new URI per tick, keys written in one transaction). It is the switch if Milestone 0 shows rename-over does not trigger a reload on some GNOME version.

## Architecture

Functional core, imperative shell. One task owns all state and runs a pure `step`; adapters turn the outside world into events, and an interpreter executes the effects `step` returns. GSettings lives on its own GLib thread, because it is `!Send` and signals on its thread-default context.

&#91;embedded content: gpaperd architecture · 6 adapters, 1 pure core, 6 effects\]

Effect results come back as events (for example `Published { style, item, result }`), so retries and "try the next candidate" stay in the core.

```rust
enum Event {
    Fired(Fired),                                   // timer: deadline or clock step
    ConfigChanged(Result<Config, ConfigError>),
    Fs(SourceId, Vec<FsChange>),
    Probed(ItemId, Check),
    Fetched(Url, Outcome),
    Published { style: Style, item: ItemId, result: Result<Method, String> },
    Gnome(GnomeEvent),                              // style or key change
    Control(Command, oneshot::Sender<Reply>),
    Shutdown,
}
enum Effect { Publish { style: Style, item: ItemId }, Republish(Style), SetKeys, Arm(Option<SystemTime>), Probe(ItemId), Fetch(Url), Persist, Report(Status) }

fn step(model: &mut Model, now: SystemTime, ev: Event) -> Vec<Effect>;
```

`now` is an argument and the RNG is a seeded field of `Model`, so the core has no clock and no hidden randomness. Property tests replay event sequences deterministically.

**Threads.** (1) A tokio current-thread runtime runs the core, the interpreter, the timer, `reqwest` and `zbus`; `notify` delivers on its own thread into a channel. (2) The GLib thread owns GSettings and `GNetworkMonitor`, with `async-channel` in and tokio `mpsc` out. (3) glycin spawns sandboxed loader processes per probe.

**Workspace.**

```text
gpaperd/
  crates/core/    model, step, ordering, scheduling: no I/O, no async
  crates/daemon/  adapters + interpreter  -> bin gpaperd
  crates/ctl/     D-Bus client            -> bin gpaperctl
  dist/           gpaperd.service, config.example.toml
```

## Config dotfile

One TOML file at `$XDG_CONFIG_HOME/gpaperd/config.toml` (override with `--config`). Each style lists any number of sources; general settings can be overridden per style. Unknown keys are errors.

```toml
# ~/.config/gpaperd/config.toml
[general]
interval = "15m"                 # 1m ..= 1h
order = "random"                 # "random" | "alphabetical"
picture_options = "zoom"         # optional; omit to leave GNOME's value alone
on_external_change = "pause"     # "pause" | "reassert-next-tick" | "reassert-now"
rotate_hidden = false            # rotate the style that is not on screen
empty_style = "use-other-style"  # or "keep-current"
remote_refresh = "startup"       # "startup" | a duration like "24h" | "never"
link = true                      # hardlink or reflink when possible; false = always copy

[light]
sources = [
  "~/Pictures/Wallpapers/light",                         # dir or file, decided at scan time
  { dir = "~/Pictures/Wallpapers/shared", recursive = true },
  { file = "~/Pictures/hero.jxl" },
  "https://example.com/wallpapers/dawn.jpg",
]

[dark]
interval = "30m"                 # per-style override
order = "alphabetical"
sources = [
  { dir = "/mnt/nas/wallpapers/dark", poll = "5m" },     # network fs: no inotify
  { url = "https://example.com/wallpapers/night.png" },
  "file:///srv/wall/aurora%20night.png",
]
```

**Source forms.** A plain string is classified by prefix: `http://` or `https://` is a URL, `file://` is decoded to a path, anything else is a path. Whether a path is a directory or a file is decided at scan time, so a path that does not exist yet is not a config error. Tables add options: `dir` takes `recursive`, `hidden`, `follow_symlinks` (default true) and `poll`.

**Path expansion.** `~` and `$VAR` / `${VAR}` are expanded; an undefined variable is an error. Relative paths resolve against the directory of the *resolved* config file, so stow or chezmoi symlinks behave.

**Validation.** Intervals parse with humantime (`90s`, `15m`, `1h`) and must be within 1 min..=1 h inclusive. URLs must be `http`/`https` with a host. A style with no sources is valid and falls under `empty_style`. Errors carry the TOML line and column.

**Load and reload rules.**

- Missing file at startup: run idle and watch for it to appear (dotfile tools may create it after login).
- Invalid at startup: exit with code 78 (`EX_CONFIG`); the unit's `RestartPreventExitStatus=78` stops a restart loop.
- Invalid on reload: keep the running config, log the error with line and column.
- Reload trigger: inotify on the config directory *and*, when the file is a symlink, on the target's directory. Debounce 300 ms and re-resolve the symlink each time, because editors save by rename.
- Applying a reload is a diff: changed sources rescan that style; a changed interval sets the deadline to `last_change + new_interval`; a changed order resets the bag or cursor; a changed `picture_options` writes that one key.

`gpaperd check-config` parses, resolves and scans without touching GNOME, prints counts per source, and exits non-zero on errors. It can run in dotfile CI.

## Sources, catalog and ordering

Each style has its own catalog, kept current by inotify rather than rescanned per tick. Images are validated just before use, and the next one is pre-validated so a tick never waits.

**Catalog item.** `Item { id, origin: Local { path, dev, ino, size, mtime } | Remote { url, blob }, sort_key, check }`. The id is the canonical path for local files and the URL for remote ones. Within a style, items are de-duplicated by `(dev, ino)`, so symlinks, hardlinks and overlapping folders count once. The same file may appear in both styles.

**Scanning.** Use `walkdir` (loop-safe when following symlinks), depth limit 16. Filter by extension first: `jpg jpeg png webp jxl avif heic heif tif tiff bmp gif qoi`. Skip hidden entries unless `hidden = true`, and in-progress names (`.part`, `.crdownload`, `.tmp`, `~`). A scan is only `stat` calls, so 10k files is cheap.

**Validation, just in time.** Before an item becomes current:

1. It still exists, is a regular file and is readable.
2. Its magic bytes are an image (`infer`), whatever the extension says.
3. A `glycin` probe succeeds. That is the same sandboxed loader stack gnome-shell 51 uses, so "probe OK" means "shell can show it". Whether header info is enough or a first-frame decode is needed gets decided in M1.

Results are cached by `(dev, ino, size, mtime_ns)`. A failed item is marked `Bad { reason, retry_after }`; a truncated file (still being copied) is retried after 10 s, up to 3 times.

**Random order: shuffle bag.** Every item is shown once per round, in a fresh random permutation. The first item of a new round is never the one on screen. New items join the current round at a random position; removed items are skipped when reached. The bag is persisted, so a restart continues the round.

**Alphabetical order: cursor.** Natural, case-insensitive sort on the file name (a URL's last path segment), ties broken by full id. The cursor is the id of the current item. "Next" is the first item sorting strictly after it, wrapping around. So a deleted or renamed current item does not restart the list from the top.

**Watching.** `notify` 8 with `notify-debouncer-full` (500 ms) on each source directory, recursive when configured.

- A missing directory: watch its nearest existing ancestor and switch over when it appears.
- A removed or unmounted directory: drop its items, keep the slot as it is, fall back to the ancestor watch.
- `inotify_add_watch` fails with `ENOSPC` (watch limit): poll that source every 5 min and log the `fs.inotify.max_user_watches` hint.
- Network or FUSE filesystems (`statfs` type NFS, CIFS/SMB2, FUSE): poll by default, because changes made elsewhere raise no inotify events. A poll is an incremental diff of `path → (dev, ino, size, mtime)`.

The spike's `ShuffleBag` and `next_alphabetical` pass tests for full coverage per round, no immediate repeats over 50 rounds, skipping removed ids, single-item catalogs, natural order and resume-after-deleted.

## Remote images: download and cache

Remote URLs are fetched at daemon start into a content-addressed cache, but rotation never waits for the network. Cached copies are used at once and revalidated in the background with conditional GETs.

**Layout.** `$XDG_CACHE_HOME/gpaperd/remote/<blake3(url)>/` holds `meta.toml` (url, ETag, Last-Modified, blob name, MIME type, fetched\_at, last\_error) and the blob `<blake3(content)>.<ext>`. A new version gets a new blob name, so it never overwrites a file a slot may be hardlinked to.

**Start-up sequence.**

1. Load every `meta.toml`; entries with a valid blob join the catalog immediately (stale-while-revalidate).
2. Revalidate all URLs in the background: at most 4 at once, one fetch per URL even if both styles list it.
3. Apply the first wallpaper from whatever is available, then signal `READY`. If a style has only remote sources and an empty cache, it keeps its current slot until the first blob lands.

`remote_refresh` sets the policy: `startup` (default) revalidates once per daemon start, a duration also revalidates on that period, `never` only fetches what is missing.

| Response or condition | Action |
| --- | --- |
| 200 with image bytes | Stream to a temp file with a running size count, `fsync`, rename to the blob, rewrite `meta.toml` the same way |
| 304 Not Modified | Update `fetched_at` only |
| 404 or 410 | Mark gone, keep using the stale blob, warn once |
| 429, 5xx, network error | Exponential backoff with jitter, 30 s up to 1 h; honour `Retry-After` |
| 200 but not an image (captive portal HTML) | Reject by magic bytes; keep the old blob |
| `Content-Length` or body over 64 MiB | Reject |
| More than 5 redirects, or https → http | Reject |
| `ENOSPC` while writing | Delete the temp file, keep the old blob, retry later |

**Timeouts.** Connect 10 s, idle read 30 s (stops slow-loris servers), total 300 s.

**Offline at login.** A user unit cannot order itself after `network-online.target`, which is a system target. Instead the GLib thread watches `GNetworkMonitor` and re-queues failed fetches when connectivity becomes full.

**Proxy.** reqwest reads `HTTP(S)_PROXY` from the environment. GNOME's own proxy settings (`org.gnome.system.proxy`) are not in a user unit's environment; resolving them via `gio::ProxyResolver` is a later option.

**Safety.** Only `http`/`https`. URLs are logged without credentials or query. Image decoding happens in glycin's sandbox, not in the daemon.

**Garbage collection.** At start and daily: delete entries whose URL left the config more than 7 days ago, and blobs no `meta.toml` references. Deleting a blob never affects a slot, because a hardlinked slot keeps its own reference to the data.

The spike's `fetch.rs` (conditional GET, size cap, magic check, atomic publish) compiles but has not run against a server; M3 tests it with `wiremock`.

## Applying wallpapers: slots, keys, style switches

Keys are written once and point at two slot files; a rotation is one atomic `rename(2)` of new content onto the visible style's slot. Two traps from the source reading shape the details: stale textures for hidden slots, and `rename` between two links of one inode doing nothing.

**Slots.** `$XDG_DATA_HOME/gpaperd/slots/light` and `…/dark`, with no extension. They live under data, not cache, so a cache cleaner cannot blank the desktop. The daemon watches the slots directory and recreates a slot that disappears.

**Key setup (start, reload, reassert).** On the GLib thread: `delay()`, set `picture-uri` and `picture-uri-dark` to the slot URIs, set `picture-options` if configured or if it is `none`, then `apply()` and `Settings::sync()`. That is one dconf transaction, and dconf drops it when nothing changed. Before the first write, the user's original keys are saved in the state file for `gpaperctl restore`.

**Publish (one rotation of style S).**

1. Take the next candidate from the bag or cursor and validate it.
2. Stage `slots/.S.tmp-<pid>`: hardlink, else reflink (`FICLONE`: btrfs, XFS, bcachefs), else copy.
3. `rename(tmp, slots/S)`. gnome-shell never sees a partial file.
4. Record `current[S]`, `last_change[S]` and the slot's `(dev, ino)`; persist state.

On any failure: remove the temp file, mark the candidate, try the next one (5 attempts at most), else keep the current slot.

**Same-inode trap.** `rename(2)` between two hardlinks of the same inode succeeds and does nothing (verified in the sandbox). No inotify event means no reload. So when the source and the slot already share an inode, staging must create a new inode (reflink or copy). The spike's `publish` does this and has a test for it.

```rust
pub fn publish(src: &Path, slot: &Path) -> io::Result<Method> {
    let dir = slot.parent().expect("slot has a parent");
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp-{}", slot.file_name().unwrap().to_string_lossy(), std::process::id()));
    let _ = fs::remove_file(&tmp); // leftover from a crash
    let same_inode = match (fs::metadata(src), fs::metadata(slot)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    };
    let method = stage(src, &tmp, !same_inode).inspect_err(|_| { let _ = fs::remove_file(&tmp); })?;
    fs::rename(&tmp, slot).inspect_err(|_| { let _ = fs::remove_file(&tmp); })?;
    Ok(method)
}
```

**Hardlink semantics.** The slot shares the original's inode. Deleting the original leaves the slot intact. Editing the original in place changes the slot's bytes, which the shell picks up on its next reload; that is acceptable.

**Which style rotates.** The daemon tracks the style the way the shell does: dark only for `prefer-dark`. With `rotate_hidden = false` (default), only the visible style's timer runs. The hidden slot keeps the last image shown in that style, which is also what the shell may have cached, so nothing goes stale.

**Hidden-slot refresh.** A slot written while hidden may meet a stale texture for the same URI from earlier in the session. That happens at start, after a config change, when its image is deleted, or always with `rotate_hidden = true`. Mark the slot `dirty_while_hidden`. When the style switches to it, wait 500 ms for the shell to build the new background and attach its file watch, then republish the same content as a new inode. That forces purge and reload, at the cost of a possible second 1 s fade at that switch.

**External changes.** Per-key `changed` handlers can see a half-applied transaction: in the spike (keyfile backend) one saw the new light key next to the old dark key. gio-rs does not bind `change-event`, so coalesce: the first `changed` schedules one task on the GLib thread's own context, which then reads both keys. Note that `glib::idle_add_local_once` targets the global default context and never ran on that thread; `MainContext::ref_thread_default().spawn_local` does. With that, the spike emits one consistent event. A change is external when a key no longer equals its slot URI.

- `pause` (default): stop rotating the affected style; resume on `gpaperctl resume`, a config reload, or when the keys point at the slots again.
- `reassert-next-tick`: write the keys back at the next tick.
- `reassert-now`: write them back after 1 s. More than 3 flips a minute means another tool is fighting; switch to `pause` and log it.

**Locked or missing keys.** If `is_writable` is false but the locked values already equal the slot URIs, rotation still works, because it only writes files. Otherwise log and idle. If the schema has no `picture-uri-dark` (GNOME < 42), run the light style only.

## Scheduling and time

Deadlines are wall-clock instants served by one `timerfd` on `CLOCK_REALTIME`, so the daemon wakes only when a change is due and stays correct across suspend and clock steps. The interval counts time a style is on screen.

**Why not `tokio::time`.** Rust's `Instant` uses `CLOCK_MONOTONIC` on Linux, which stops during suspend. A 1 h sleep armed before a 30 min suspend fires 30 min late. An absolute `CLOCK_REALTIME` deadline that passes during suspend fires right after resume.

**Timer.** One `timerfd`, flags `TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET`, armed for the earliest active deadline. A clock step (NTP, manual change) makes `read` fail with `ECANCELED`: recompute and re-arm. Time-zone changes do not move `CLOCK_REALTIME`. There is no polling; the extension wakes every ≤ 60 s instead.

```rust
pub async fn wait(&self) -> std::io::Result<Fired> {
    loop {
        let mut guard = self.fd.readable().await?;
        let mut buf = [0u8; 8];
        match rustix::io::read(self.fd.get_ref(), &mut buf) {
            Ok(_) => return Ok(Fired::Deadline),
            Err(Errno::CANCELED) => return Ok(Fired::ClockChanged), // caller re-arms
            Err(Errno::AGAIN) => guard.clear_ready(),
            Err(e) => return Err(e.into()),
        }
    }
}
```

A zero `it_value` disarms a timerfd, so a deadline at or before the epoch is clamped to 1 ns. Any past deadline fires at once (tested). tokio 1.54 deprecates `AsyncFd::with_interest`; use the `unsafe` `register_with_interest` with an `OwnedFd`.

**Deadline rules.**

- `deadline[S] = last_change[S] + interval[S]`.
- Clock went back (`now < last_change`): set `last_change = now`, a fresh interval.
- A style becoming hidden stores `remaining[S]`; becoming visible sets `deadline = now + remaining`. So a style switch never forces an immediate change. With `rotate_hidden = true` both clocks keep running.
- Overdue at start (daemon was off): rotate once after the first apply.
- Interval changed by reload: `deadline = last_change + new_interval`; if already past, rotate now.
- Unpause (user, external-change policy, unlock): if overdue, rotate now; otherwise keep the deadline.

**Optional pauses.** `pause_when_locked` listens to `org.gnome.ScreenSaver` `ActiveChanged` on the session bus; nobody sees a change behind a locked, blanked screen. `logind` `PrepareForSleep` is not needed for timing; it is only a hook if a resume policy is ever wanted.

**State file.** `$XDG_STATE_HOME/gpaperd/state.json` holds a schema version, and per style: current id, slot `(dev, ino)`, `last_change`, `remaining`, bag queue, `dirty_while_hidden`, paused flag. It also holds the user's original keys and a config hash. It is written via temp file, `fsync` and rename after each rotation and on `SIGTERM`, so at most once a minute. A corrupt or unknown version is logged and replaced; the slots are kept.

## Corner cases and failure handling

The rule behind every row: never show a blank or broken desktop, never fight the user, never block on the network. When in doubt, keep the current slot and log once.

| Area | Situation | Detection | Handling |
| --- | --- | --- | --- |
| Config | File missing at start | `ENOENT`, inotify on its dir | Run idle; start when it appears |
| Config | Invalid at start | Parse or validation error | Exit 78; unit does not restart |
| Config | Invalid on reload | Parse or validation error | Keep running config; log line and column |
| Config | Symlinked dotfile changes in its repo | Watch the target's dir; re-resolve | Reload |
| Config | Interval outside 1 min..=1 h | Validation | Error, as above |
| Config | Both styles end up with zero usable images | Catalog sizes | Keep both slots as they are; warn; idle |
| Local | Source dir missing, created later | `ENOENT`; ancestor watch | Add its items when it appears |
| Local | Source dir deleted or unmounted | `IN_DELETE_SELF`, `IN_UNMOUNT` | Drop items; slot keeps its data; watch ancestor |
| Local | Permission denied | `EACCES` | Skip; warn once per path |
| Local | Broken symlink or symlink loop | walkdir error | Skip |
| Local | Not an image, corrupt, or no loader (HEIC without the glycin loader) | Magic bytes + glycin probe | Mark bad with reason; skip |
| Local | File still being copied in | Probe fails; size changing | Retry after 10 s, 3 times |
| Local | Current image deleted | inotify | Nothing visible: hardlinked or copied slot keeps the data; next tick moves on |
| Local | Current image edited in place | `IN_CLOSE_WRITE` | Republish as a new inode so the shell reloads |
| Local | Current image replaced by an atomic save | `IN_MOVED_TO` | Update the item; republish if it is current |
| Local | Same file via two paths | `(dev, ino)` | One item |
| Local | Only one image in a style | Catalog size 1 | Publish once; no rotation |
| Local | Very large image | glycin image info | Skip above `max_pixels` (default 100 MP) |
| Local | NFS, SMB, FUSE source | `statfs` type | Poll every 5 min |
| Local | inotify watch limit hit | `ENOSPC` | Poll that source; log the sysctl hint |
| Local | Source on another filesystem than the slots | `EXDEV` on link | Reflink, else copy |
| Remote | Offline at login | Fetch error | Use cache; retry on connectivity change |
| Remote | 404 or 410 | Status | Keep the stale blob; warn once |
| Remote | Captive portal returns HTML | Magic bytes | Reject; keep the old blob |
| Remote | Slow or huge response | Timeouts, 64 MiB cap | Reject; back off |
| Remote | Disk full | `ENOSPC` | Keep the old blob; retry later |
| Remote | Cache dir wiped at runtime | Blob missing on use | Refetch; slots unaffected |
| GNOME | Not a GNOME session, schema missing | Schema lookup before `Settings::new` (which aborts) | Exit 69 (tested); the unit's `ExecCondition` usually prevents the start |
| GNOME | `GSETTINGS_BACKEND=memory`, no dconf | Backend type name | Exit 69 (tested) |
| GNOME | Keys locked by an administrator | `is_writable` | Work if they already point at the slots; else log and idle |
| GNOME | User picks a wallpaper in Settings | Key ≠ slot URI | `on_external_change` policy; default pause |
| GNOME | Another rotator keeps rewriting keys | > 3 flips a minute | Pause and log |
| GNOME | `picture-options` is `none` | Probe | Set `zoom` |
| GNOME | Switch to a style whose slot was written while hidden | `dirty_while_hidden` | Republish 500 ms after the switch |
| GNOME | `color-scheme` is `default` | Same predicate as the shell | Light |
| GNOME | Several monitors | Monitor layout | GNOME shows one image on all; `spanned` is available; per-monitor images are out of scope |
| Slots | Slot file deleted by someone | inotify on the slots dir | Republish the current item at once |
| Slots | Hardlink refused (`fs.protected_hardlinks`, `EMLINK`) | `EPERM`, `EMLINK` | Reflink, else copy |
| Slots | Next item shares the slot's inode | `(dev, ino)` compare | Stage a new inode (tested) |
| Slots | Temp files left by a crash | Start-up sweep | Delete `.*.tmp-*` |
| Time | Suspend and resume | `CLOCK_REALTIME` deadline | Fires on resume if due |
| Time | Clock stepped | `ECANCELED` | Recompute; re-arm |
| Time | Clock went backwards | `now < last_change` | Fresh interval |
| Process | Second instance | D-Bus name already owned | Exit 0 with a message |
| Process | `SIGTERM` | Signal | Persist state; exit 0 within 5 s |
| Process | `SIGKILL` during a write | Cannot be caught | Temp file + rename everywhere: no partial state, slot or blob |
| Process | Crash loop | systemd start limit | 5 starts per 60 s, then failed |
| Process | Logout | `PartOf=graphical-session.target` | Stopped with the session |
| Process | GNOME Shell restarts | Shell restart | Shell reloads from the keys; slots intact |

## systemd user unit

A `Type=notify` user service tied to `graphical-session.target`, which gnome-session starts and stops with the session. It skips itself outside GNOME and never restart-loops on a config or environment error.

```ini
# ~/.config/systemd/user/gpaperd.service
[Unit]
Description=GNOME light/dark wallpaper rotator
PartOf=graphical-session.target
After=graphical-session.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=notify
NotifyAccess=main
# Exit 1-254 here means "skip", not "fail" (systemd >= 243).
ExecCondition=/bin/sh -c 'case ":$XDG_CURRENT_DESKTOP:" in *:GNOME:*) exit 0;; esac; exit 1'
ExecStart=%h/.cargo/bin/gpaperd
Restart=on-failure
RestartSec=3
# 69 = not GNOME / volatile GSettings, 78 = bad config: restarting cannot help.
RestartPreventExitStatus=69 78
TimeoutStartSec=30
TimeoutStopSec=5
Slice=background.slice
Nice=10
IOSchedulingClass=idle
NoNewPrivileges=yes
# Optional, test first: in user units these imply PrivateUsers=yes and
# need unprivileged user namespaces.
#ProtectSystem=strict
#ReadWritePaths=%h/.local/share/gpaperd %h/.local/state/gpaperd %h/.cache/gpaperd

[Install]
WantedBy=graphical-session.target
```

**Readiness.** `READY=1` goes out after config load, the GNOME probe and the first apply. The first apply uses the state file's current items, so a large or network-mounted folder does not delay it; the full scan continues afterwards. `STATUS=` carries a one-line summary (current files, next change). `STOPPING=1` goes out on `SIGTERM`.

**Logging.** `tracing` with `tracing-journald` when `JOURNAL_STREAM` is set, otherwise formatted to stderr. Each event carries structured fields: `style`, `item`, `method` (hardlink, reflink, copy), `reason`.

**Install.**

```sh
cargo install --path . --locked
install -Dm644 dist/gpaperd.service ~/.config/systemd/user/gpaperd.service
systemctl --user daemon-reload
systemctl --user enable --now gpaperd.service
journalctl --user -u gpaperd -f
```

The unit file belongs in the dotfiles repo next to `config.toml`.

## Control CLI and D-Bus API

A small session-bus interface served with `zbus`, plus a `gpaperctl` client binary in the same crate (the wpaperd / wpaperctl split). Owning the bus name with `DoNotQueue` also makes the daemon single-instance.

Name `io.github.<owner>.Gpaperd1`, object `/io/github/<owner>/Gpaperd1`. Each method sends a command plus a `oneshot` reply channel into the core's event queue; the core stays the only owner of state.

| Member | Kind | Signature | Effect |
| --- | --- | --- | --- |
| `Next` | method | `(s style) → ()` | Rotate now; style is `light`, `dark`, `visible` or `both` |
| `Previous` | method | `(s style) → ()` | Step back through a 20-entry history per style |
| `Pause`, `Resume` | method | `(s style) → ()` | Stop or restart a style's timer |
| `Reload` | method | `() → (b ok, s error)` | Re-read config; the error text has line and column |
| `RefreshRemote` | method | `() → ()` | Revalidate every URL now |
| `Restore` | method | `() → ()` | Write back the user's original keys and pause both styles |
| `Status` | method | `() → (s json)` | Style, current ids, next change, paused styles, catalog sizes, bad items |
| `Style`, `Paused`, `CurrentLight`, `CurrentDark`, `NextChange` | property | `s`, `as`, `s`, `s`, `t` (unix s) | Emit `PropertiesChanged` |
| `Changed` | signal | `(s style, s id)` | After every successful publish |

CLI mapping: `gpaperctl next [--style visible]`, `previous`, `pause`, `resume`, `reload`, `refresh`, `restore`, `status [--json]`. `gpaperd check-config` stays in the daemon binary because it needs no running daemon.

## Crates and versions

Versions are the latest stable on crates.io as of 2026-11-14; the spike built with Rust 1.97.0, edition 2024. One deliberate C dependency: `gio`, because GSettings already handles schema defaults, admin locks, backend selection and change signals. Doing that in pure Rust would mean parsing dconf's GVDB user and system databases.

| Crate | Version | Used for | Note |
| --- | --- | --- | --- |
| `tokio` | 1.54.1 | current-thread runtime, signals, `AsyncFd` | `AsyncFd::with_interest` is deprecated; use `register_with_interest` |
| `gio`, `glib` | 0.22.10 | GSettings, `GNetworkMonitor` on a dedicated thread | Links system GLib; `change-event` is not bound |
| `async-channel` | 2.5.0 | Commands into the GLib thread | Works with `glib::MainContext::spawn_local` |
| `zbus` | 6.3.0 | Control API, `org.gnome.ScreenSaver`, `org.gnome.Shell` version | Not compiled in the spike |
| `rustix` | 1.1.6 (`fs`, `time`) | `timerfd`, `FICLONE`, `statfs` |  |
| `notify`, `notify-debouncer-full` | 8.2.2, 0.7.0 | inotify watching | 0.8 is still a release candidate |
| `walkdir` | 2.5.0 | Scanning |  |
| `reqwest` | 0.13.5 (`stream`) | HTTP | Default TLS backend |
| `infer` | 0.22.0 | Magic bytes |  |
| `glycin` | 4.0.5 (`tokio`) | Validation with the shell's loaders | Spawns sandboxed loader processes |
| `blake3` | 1.8.7 | Cache keys and blob names |  |
| `serde`, `toml` | 1.0.229, 1.2.1 | Config, state, cache metadata |  |
| `humantime` | 2.4.0 | Interval parsing |  |
| `url` | 2.5.8 | `file://` URIs | Percent-encodes `#`, `%`, spaces (tested) |
| `rand` | 0.10.3 | Shuffle bag | `random_range` now lives on `RngExt` |
| `natord` | 1.0.9 | Natural sort | Unchanged since 2015; vendoring \~40 lines is fine |
| `sd-notify` | 0.5.0 | Readiness, status | `notify(&[NotifyState::Ready])` |
| `tracing`, `tracing-subscriber`, `tracing-journald` | 0.1.44, 0.3.23, 0.3.2 | Logging |  |
| `clap` | 4.6.7 | Both binaries |  |
| `thiserror`, `anyhow` | 2.0.21, 1.0.104 | Errors |  |
| `proptest`, `wiremock`, `tempfile` | 1.12.0, 0.6.5, 3.27.0 | Tests |  |

XDG directories: read `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` with the spec's fallbacks directly; it is a few lines and avoids a dependency.

## Testing and verification

Most behaviour is tested without GNOME, because the core is a pure state machine and GNOME sits behind a port trait. Only the shell-facing assumptions need a real session, and Milestone 0 checks those first.

**Already verified in the spike** (Rust 1.97.0; 10 tests pass):

- Config: parsing, path resolution, `file://` decoding, interval bounds (`30s` and `61m` rejected), unknown keys and `ftp://` rejected.
- Ordering: shuffle bag covers every item per round with no immediate repeat over 50 rounds; skips removed ids; natural sort; alphabetical resume after a deleted item.
- Slots: atomic replace, data survives deletion of the source, no temp files left, failed publish keeps old content, same-inode republish gets a new inode, URI encoding.
- Timer: fires at an absolute deadline; overdue deadlines fire at once.
- GSettings adapter (temp schemas, keyfile backend): probe; both keys in one transaction; one coalesced change event; exit 69 for a missing schema and for the memory backend.

**Test layers to build.**

1. **Unit (pure).** Everything above plus scheduler maths: deadlines, clock going back, overdue start, `remaining` across hide and show.
2. **Property tests on the reducer.** `proptest` generates event sequences: ticks, files added and removed, style switches, external key changes, reloads. Invariants: only validated, present items are published; with `rotate_hidden = false` the hidden slot is touched only for repairs; no key writes while paused under `pause`; the armed deadline is within `[now, now + interval]`; a switch to a dirty slot yields exactly one republish.
3. **Integration without GNOME.** A fake `GnomePort` with in-memory keys and scripted style changes. Real files in temp dirs: hardlink, `EXDEV` (tmpfs vs disk), same inode, and inotify cases (create, delete, move-in, atomic save, directory removal). `wiremock` for 200, 304, 404, 429 with `Retry-After`, 500, HTML, oversize and slow bodies.
4. **GSettings adapter.** Compiled test schemas plus `GSETTINGS_BACKEND=keyfile`, as in the spike.
5. **Real GNOME (M0, then before each release).** A GNOME 51 VM and a GNOME 50 VM. Checks: rename-over reloads the visible style within 2 s; a hidden slot rewritten then switched to is stale until the refresh fixes it within 1.5 s; `gio info -a standard::content-type` on an extensionless slot reports an image type; writing only the hidden key causes a visible fade.
6. **Memory measurement (M0).** 50 rotations of distinct 4K images, slot mode vs URI mode, recording gnome-shell RSS and GPU memory (`intel_gpu_top`, `radeontop` or `nvidia-smi`). Expected on GNOME 51: flat for slots, roughly +33 MB per distinct 4K image for URIs (3840 × 2160 × 4 bytes).
7. **Soak (24 h, 1 min interval).** A churn script adds, deletes and renames files every few seconds; `nmcli` toggles the network; `rtcwake -m mem -s 60` suspends; `timedatectl set-time` steps the clock. Pass: no crash, no blank desktop, valid state file, flat shell memory.

**CI.** `cargo fmt --check`, `clippy -D warnings`, layers 1–4, and `cargo deny` for advisories and licences.

## Milestones

Six milestones, about 13–18 working days in total. M0 comes first because its result can change the default apply mode.

| Milestone | Scope | Exit criteria | Estimate |
| --- | --- | --- | --- |
| M0 Shell spike | Run the real-GNOME checks and the memory measurement from the testing section on GNOME 51 and 50 | Gate: if rename-over reloads, slot mode is the default; otherwise `uri` mode | 1–2 d |
| M1 Core | Workspace layout, config, XDG paths, local scan and catalog, ordering, reducer and effect interpreter, slots, GNOME adapter, `timerfd`, state file, `sd_notify`, journald | Both styles rotate from local folders on GNOME 51; order survives a restart; test layers 1–4 green | 4–5 d |
| M2 Robustness | inotify and polling, missing dirs, hot reload, external-change policies, style tracking and hidden-slot refresh, slot watch, temp sweep, glycin validation | Config, Local, GNOME, Slots and Time rows of the corner-case table covered by tests; M0 checks pass | 3–4 d |
| M3 Remote | Fetcher, cache, revalidation, backoff, connectivity retry, GC | `wiremock` suite green; an offline login rotates from cache | 2–3 d |
| M4 Control | `zbus` interface, `gpaperctl`, history for `Previous`, `Restore` | Every CLI command works against a running daemon; a second instance exits 0 | 1–2 d |
| M5 Polish | `pause_when_locked`, `check-config`, man pages, unit file, README | 24 h soak passes | 2 d |

## Decisions, assumptions, open questions, risks

**Decisions made on your behalf** (each is a config switch or a small change):

- "Writes both `picture-uri` and `picture-uri-dark`" is met by pointing each key at its own style's slot. Keys are written once; content rotates. `apply_mode = "uri"` gives literal per-tick key writes.
- The interval is global with a per-style override, valid from 1 min to 1 h inclusive.
- The hidden style does not rotate by default, and its interval counts on-screen time only.
- "Downloaded and cached on start" means: cached copies are used at once and every URL is revalidated at each daemon start.
- An external wallpaper change pauses rotation; a 404 keeps the stale cached image.

**Assumptions still to verify (all in M0 or M1):**

- Rename-over makes GNOME 50 and 51 reload the slot. The code says yes; not yet observed.
- GNOME 51's texture cache grows with distinct URIs. Code reading only; M0 measures it.
- An extensionless slot is detected as an image.
- The `glycin` crate accepts the same files as the shell. That holds only if it uses the system's glycin loaders, as the shell does.
- Sources and slots usually share a filesystem, so the hardlink path is the common one.

**Open questions for you:**

1. Should the hidden style rotate too (`rotate_hidden`)?
2. On a style switch, show that style's last image (planned) or a fresh one?
3. Per-monitor images? GNOME has no per-monitor wallpaper setting (XML slideshows only pick a size variant per monitor), so this is out of scope today.
4. Distribution: `cargo install` only, or packages (AUR, RPM, deb)?
5. A final name; `gpaperd` is a placeholder.

**Risks.**

| Risk | Effect | Mitigation |
| --- | --- | --- |
| GNOME changes background loading again (it was rewritten for 51) | Slot reload or refresh stops working | Script the M0 checks and rerun them per GNOME release; `uri` mode fallback |
| Upstream adds eviction to the texture cache | None for correctness | Slot mode stays valid; URI mode becomes cheaper |
| Dirty hidden slot on a style switch | A second 1 s fade at that switch | Rare; only after start, reloads or deletions |
| glycin sandbox (`bwrap`) unavailable, for example in nested containers | Probe fails for every file | Fall back to magic-bytes validation with one warning |
| Hardlinked slot | In-place edits of an original show on the desktop at the next reload | Accept, or set `link = false` to always copy |
| Another wallpaper tool or extension | Keys flip back and forth | Flip detection pauses rotation and logs |

If M0 confirms the cache growth, it is worth reporting to gnome-shell with the measurement: `BackgroundTextureCache` has no eviction besides file-change purges.

## Sources

Source files were cloned and read at the commits linked below (GitHub mirrors of GNOME GitLab).

- [shell-wallpaper-rotator @ 4101af9](https://github.com/lsantiagoba/shell-wallpaper-rotator/tree/4101af97606da1bff2a7ef29a6bbe2002b5894f0)
- [gnome-shell `js/ui/background.js` @ 7fcbec8 (51.1)](https://github.com/GNOME/gnome-shell/blob/7fcbec8013686024aca572b2bcdbc8102fdef18a/js/ui/background.js)
- [gnome-shell commit 8b61c2a: load and cache images using glycin](https://github.com/GNOME/gnome-shell/commit/8b61c2a156b473f13925c8bb374a5574599d561a)
- [gnome-shell `js/ui/unlockDialog.js` @ 51.0](https://github.com/GNOME/gnome-shell/blob/51.0/js/ui/unlockDialog.js)
- [mutter `meta-background.c` @ be02dfa](https://github.com/GNOME/mutter/blob/be02dfae4ad96c907433f9fd514317b7514f2a36/src/compositor/meta-background.c)
- [mutter 49.0 `meta-background-image.c`](https://github.com/GNOME/mutter/blob/49.0/src/compositor/meta-background-image.c)
- [gnome-desktop `gnome-bg-slide-show.c` @ 2e9bf76](https://github.com/GNOME/gnome-desktop/blob/2e9bf76aad453b02e93ae05bca92d04575f6e7d4/libgnome-desktop/gnome-bg/gnome-bg-slide-show.c)
- [GLib `gio/glocalfilemonitor.c` @ a6457d8](https://github.com/GNOME/glib/blob/a6457d849e45b9a1a9473c61364ee6e56894774b/gio/glocalfilemonitor.c)
- [dconf `service/dconf-writer.c` @ 264e659](https://github.com/GNOME/dconf/blob/264e6598863fc1d9ad6edb4c58d86f82ea669927/service/dconf-writer.c)
- [xdg-desktop-portal-gnome `src/wallpaper.c` @ fd1e79f](https://github.com/GNOME/xdg-desktop-portal-gnome/blob/fd1e79fa2c7e84a27da9f6327e7f1f924fe85cd9/src/wallpaper.c)
- [gsettings-desktop-schemas background schema @ 77fa51b](https://github.com/GNOME/gsettings-desktop-schemas/blob/77fa51b13fd6dafc351436ad45884679477b5307/schemas/org.gnome.desktop.background.gschema.xml.in)
- [systemd.exec(5): sandboxing in user units implies `PrivateUsers=`](https://www.freedesktop.org/software/systemd/man/devel/systemd.exec.html)
- [gnome-session(1): `graphical-session.target`](https://man.archlinux.org/man/gnome-session.1.en.txt)
- [GNOME Discourse: user service with `graphical-session.target`](https://discourse.gnome.org/t/start-a-systemd-user-service-with-graphical-session-in-gnome-40/8738)
- [Phoronix: GNOME Shell 51.rc loads and caches backgrounds with glycin](https://phoronix.com/news/GNOME-Shell-Mutter-51-RC)
- Crate versions: [crates.io](https://crates.io) API, queried 2026-11-14
