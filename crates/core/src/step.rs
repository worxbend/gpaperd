//! The reducer: one pure function from (state, time, event) to effects.

mod control;
mod keys;
mod refresh;
mod reload;
mod remote;

use std::mem;
use std::time::{Duration, SystemTime};

use url::Url;

use crate::catalog::{BadReason, CatalogDelta, FileId, ItemId, Version};
use crate::config::ApplyMode;
#[cfg(doc)]
use crate::control::Reply;
#[cfg(doc)]
use crate::event::Pause;
use crate::event::{Effect, Event, Fired, GnomeEvent, GnomeSnapshot, SlotWrite};
use crate::key::Reassert;
use crate::model::{Candidate, Model, Phase, ProbeRetry, Rotation, Stage, Timer};
use crate::style::{PerStyle, Style};

/// Candidates tried per rotation before the slot is left as it is.
pub const MAX_PUBLISH_ATTEMPTS: u32 = 5;

// The delays below are wall-clock deadlines of the model, like a rotation's:
// `Effect::Arm` carries the earliest deadline of all, and `Event::Fired`
// brings the reducer back when it is due. There is no second timer.

/// How long after a switch to a style a slot that was written while hidden
/// is republished. The shell needs that long to build the new background and
/// to watch the slot file.
pub const HIDDEN_REFRESH_DELAY: Duration = Duration::from_millis(500);
/// How long `reassert-now` waits before it writes the keys back.
pub const REASSERT_DELAY: Duration = Duration::from_secs(1);
/// External key changes within one minute that `reassert-now` writes back.
/// One more pauses the style: another tool is fighting for the keys.
pub const MAX_KEY_FLIPS_PER_MINUTE: u32 = 3;
/// How long a [`BadReason::Truncated`] item waits for its next probe.
pub const PROBE_RETRY_DELAY: Duration = Duration::from_secs(10);
/// Probes a truncated item gets after its first one.
pub const MAX_PROBE_RETRIES: u32 = 3;
/// [`Effect::Fetch`]es that are unanswered at once, at most. Others wait in
/// a queue, oldest first.
pub const MAX_FETCHES_IN_FLIGHT: usize = 4;
/// How often the remote cache's garbage is collected while the daemon knows
/// of any remote image. Once is when the cache was loaded.
pub const GC_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

/// Applies one event to the model and returns what the interpreter must do,
/// in order.
///
/// Pure and deterministic: no I/O, no clock, and randomness only from the
/// model's seeded RNG. `now` is the wall-clock time at which `ev` is handled.
/// Never panics, whatever the event sequence.
///
/// One rotation of a style is: pick the next candidate (shuffle bag or
/// alphabetical cursor), [`Effect::Probe`] it, then [`Effect::Publish`] it.
/// A failed probe marks the candidate bad until its file changes; a failed
/// publish does not. Either way the rotation moves to the next candidate, up
/// to [`MAX_PUBLISH_ATTEMPTS`], and never back to one it tried. After that, or
/// when no other candidate is left, the slot is kept and a fresh interval
/// starts; a slot that was lost meanwhile gets its current item back. At most one rotation per style is in flight. A style whose only
/// usable image is on screen does not rotate and has no deadline. A shuffled
/// candidate that a rotation took and did not show, for any reason but a
/// failed probe, goes back to the front of its round.
///
/// When the file of the image in a slot changes, a rotation publishes that
/// same image again. It keeps the image's interval, it also writes a hidden
/// slot, and it has no next candidate: when it fails, the slot stays as it
/// is until the next tick.
///
/// In uri mode (`apply_mode = "uri"`) no slot file is touched. A candidate
/// that passed its probe is on screen once its own `file://` URI is in the
/// style's key, so a rotation ends with an [`Effect::SetKeys`] and has no
/// publish. Where this text says that a slot is filled, uri mode reads: an
/// image passed its probe for the style in this run. That is never so when
/// a run starts, so every start probes each style's image again. An image
/// on screen whose file leaves the catalog is replaced at once, because no
/// slot keeps its data.
///
/// Time is level-triggered. Every event from [`Event::Started`] on first
/// applies the clock-went-back rule, and afterwards starts a rotation for
/// every running style whose deadline is at or before `now`. The timer only
/// has to wake the reducer up. With `rotate_hidden = false` only the visible
/// style runs, and a style whose slot is still to be filled: filling a slot
/// is a repair, so it does not wait for the style to be shown. It is tried
/// again when an image appears for it, and every interval until it works.
///
/// The number of usable images decides what a style does, at any moment:
/// with two or more it rotates, with one it shows that one, and with none it
/// follows `empty_style`. With `use-other-style` it picks from the other
/// style's images, with `keep-current` its slot is left alone. Two styles
/// without images keep their slots and idle.
///
/// Per event:
/// - Before [`Event::Started`] every event is ignored, except that
///   [`Event::Shutdown`] ends the run without effects, an
///   [`Event::Control`] is answered with [`Reply::Failed`], and
///   [`Event::ScreenLock`] is recorded. A second `Started` is ignored.
/// - [`Event::Started`]: ask for the scan and the watch of each style's
///   sources with [`Effect::Watch`], record the visible style, and keep the
///   snapshot's keys as the original keys unless restored state already has
///   some. Then the first apply, per style, hidden or not: a slot that is
///   missing or has another inode than the restored one is still to be
///   filled, with its current item when that is still usable, else with a
///   first pick. An intact slot is left alone, even when its item left the
///   catalog. After that a running style that is overdue rotates once.
///
///   A style whose catalog is still unknown (see `ModelInit::local_files`)
///   has only the item that the state file names for its slot. The first
///   apply publishes that one when the slot needs it. Everything else waits
///   for the style's [`Event::Scanned`], except its cached remote images,
///   and readiness does not.
///
///   Before the first apply the keys are looked at. A style is left alone
///   for the run when the schema has no key for it (dark before GNOME 42,
///   [`Pause::NoKey`]), and for as long as its key is locked on anything
///   but its slot URI ([`Pause::LockedKey`]): no probe, no slot write, no
///   key write. In uri mode every locked key is such a key. A key that is
///   locked on the slot URI is fine, because a rotation only replaces the
///   slot file. A style that the last run left paused stays paused, unless
///   its key holds the daemon's value again: the slot URI, or in uri mode
///   the image URI that the last run wrote (saved). When the slot on screen
///   is dirty, its refresh is planned as after a style switch.
/// - [`Event::Scanned`] and [`Event::Fs`]: bring the style's catalog up to
///   date. A scan replaces it; a file change adds, removes or updates one
///   file. A check is kept for as long as its file's stamp stays the same.
///   - A new image joins the shuffle round in progress at a random
///     position, unless that round showed it already. In alphabetical order
///     it sorts in.
///   - A removed image leaves the catalog. It is skipped when its turn
///     comes, and the alphabetical cursor resumes after it. When it is the
///     image on screen, nothing happens until the next tick: the slot keeps
///     its data.
///   - A changed image, edited in place or replaced by an atomic save,
///     loses its check. When it is the image in a slot, it is published
///     again.
///   - A slot that shows no item of the daemon's is filled at once, now
///     that there may be an image for it.
///   - After a style's first scan, an image that its restored round neither
///     holds nor showed is new since the last run and joins that round. A
///     round from a state file that did not save what it showed counts the
///     images of that scan as shown, and takes none of them.
/// - [`Event::SlotLost`]: the slot file is gone, so the slot is filled again
///   at once: by the rotation in flight when there is one, else with its
///   current item, else with a first pick. A rotation in flight that
///   publishes nothing ends with its current item.
/// - [`Event::Fired`] with `Deadline`: the one-shot timer is spent, so it is
///   armed again when no rotation starts.
/// - [`Event::Fired`] with `ClockChanged`: the armed deadline is stale, so
///   an [`Effect::Arm`] is always emitted.
/// - [`Event::Probed`] and [`Event::Published`]: advance or retry that
///   style's rotation. A successful publish records the current item and the
///   slot inode, starts a fresh interval unless the same image was published
///   again, and emits [`Effect::Persist`]. A result nobody waits for is
///   ignored. When a style left the screen while its probe ran, the rotation
///   ends without a publish and starts again once the style is shown: a
///   hidden slot gets another image only while it is still to be filled.
///
///   A verdict is about the version of the file that was probed. When the
///   file changed since, the candidate is probed again; when it left the
///   catalog, it is dropped. An image that changed on its way into the slot
///   is published again.
///
///   [`BadReason::Truncated`] is a file that is still being copied in. Its
///   item is bad for [`PROBE_RETRY_DELAY`] and may then be probed again, up
///   to [`MAX_PROBE_RETRIES`] times per version of the file. Readiness and
///   the other images do not wait for it. Once it may be probed again, it
///   joins its round again, a slot that waited for it is filled, and a
///   slot that waited to get its new content gets it.
/// - [`Event::Gnome`] with `StyleChanged`: with `rotate_hidden = false`, stop
///   the hidden style's clock (store `remaining`) and restart the visible
///   one at `now + remaining`. A change to the style that is already visible
///   does nothing.
///
///   A slot that was written while its style was hidden is dirty: the shell
///   may hold a stale texture for it. When its style comes on screen, its
///   refresh is planned for [`HIDDEN_REFRESH_DELAY`] later. When the style
///   leaves the screen first, the plan is dropped and the mark stays.
/// - [`Event::Republished`]: the refresh is over. On success the slot's new
///   inode is recorded and saved. A failure is not tried again.
/// - [`Event::Gnome`] with `LocksChanged`, and `KeysChanged` for a locked
///   key: a lock added at runtime on anything but the slot URI stops the
///   style as at start, in place of an external-change pause. A lock that
///   is lifted, or a locked key that names the slot URI by now, lets the
///   style go on: it fills its slot when that is empty, rotates at once
///   when it is overdue, and a free key that holds someone else's value is
///   an external change. A reload into uri mode stops a style whose key is
///   locked on its slot URI.
/// - [`Event::Gnome`] with `KeysChanged`: each key is compared with the URI
///   that the daemon last wrote to it. The daemon's own writes come back
///   this way and change nothing, and a key that this run never wrote is
///   the user's. A key that held the daemon's URI and holds another now was
///   changed from outside, and `on_external_change` says what follows:
///   - `pause`: the style stops ([`Pause::ExternalChange`], saved): it does
///     not rotate and its key is not written. Repairs of its slot go on. It
///     resumes when the key holds the daemon's URI again, or on a config
///     reload. Its clock ran on meanwhile, so an overdue style rotates at
///     once and any other keeps its deadline.
///   - `reassert-next-tick`: the key is written back when the next rotation
///     of any style is due. A style with nothing to rotate to gets a tick
///     for this, one interval after the change.
///   - `reassert-now`: the key is written back after [`REASSERT_DELAY`],
///     unless it holds the daemon's URI again by then. One change more than
///     [`MAX_KEY_FLIPS_PER_MINUTE`] within a minute pauses the style, as
///     `pause` does.
/// - [`Event::ConfigChanged`] with `Err`: nothing. The running config stays.
/// - [`Event::ConfigChanged`] with `Ok`: the new config is the running one
///   from here on, and what differs is applied:
///   - `sources` of a style: [`Effect::Watch`] for that style. Its local
///     items stay as they are until the [`Event::Scanned`] of the new
///     sources; its removed URLs leave at once. Until that scan the style
///     does not turn to the other style's images.
///   - `interval`: the deadline is `last_change + interval`, so a style
///     that is overdue by the new interval rotates in this step.
///   - `order`: the shuffle round is dropped. The alphabetical cursor is the
///     image on screen, which stays.
///   - `picture_options`: a new value is written, alone when no URI is owed.
///     A value that left the config leaves the key alone.
///   - `apply_mode`: both styles are filled anew, with the images they show,
///     and the keys follow.
///   - `max_pixels`: when it went up, every "too large" verdict is dropped.
///   - URLs: a URL that no style listed is fetched as at start. One that no
///     style lists any more leaves the catalogs at once and gets an
///     [`Effect::DropRemote`].
///   - Every other setting is read when it is needed.
///
///   Any reload ends the pause of an external change and writes back every
///   key that something else holds. A daemon that started without a config
///   file (`ModelInit::config` is `None`) runs on the default config, which
///   has no sources, and leaves the slots and the keys alone: it idles
///   until the first `Ok` brings a config. Then every slot that is not
///   known to hold its current item is filled, as at a start.
/// - [`Event::CacheLoaded`]: the remote cache is known. Each style's
///   catalog gets one remote item per URL of its sources that has a blob,
///   and every listed URL is fetched once, however many styles list it:
///   with `remote_refresh = "never"` only a URL without a blob. A second
///   report is ignored. Until the first, nothing is fetched, and a style
///   whose URLs may still bring an image does not turn to the other
///   style's images: with no image of its own it keeps its slot.
/// - [`Event::Fetched`]: a new blob adds or updates its URL's item in every
///   catalog that lists it, like a new or changed file: the image on screen
///   is published again. Any other answer leaves the catalogs alone. `Gone`
///   ends the wait for a URL's first image. `Failed` plans the next fetch
///   at its backoff, and with a refresh period every other answer plans one
///   a period later. An answer that no fetch waits for is ignored.
///
///   A probe that finds a remote item's blob missing takes the item out of
///   the catalogs and fetches its URL again. The slot keeps its data.
/// - [`Event::Gnome`] with `ConnectivityChanged` to full: every URL whose
///   last fetch failed is fetched again now.
/// - [`Event::Control`]: every request gets exactly one [`Effect::Reply`]
///   with its id. `visible` means the style on screen when it is handled.
///   - `Next` rotates each style meant now, through the normal probe and
///     publish, and replies once every one of them is over: `Done` when
///     each shows another image, else `Failed`. A style with a rotation in
///     flight, or an earlier request, waits for it. A requested rotation
///     may write a hidden slot, which is then dirty. A style without
///     another usable image, without a config, or whose key is missing,
///     locked or not the daemon's (an external-change pause, or the user's
///     own value after `Restore` or a restart under a user pause) fails at
///     once: no image is shown that the key does not name.
///   - Every publish of another image pushes the one it replaced onto the
///     style's history (20 entries, saved). `Previous` pops it and shows the
///     newest entry that is still usable, skipping gone or failing ones,
///     and does not push what it replaces: two `Previous` go back two
///     images. `Next` always draws by the order; it does not walk forward
///     through the history.
///   - `Pause` sets the user pause ([`Pause::User`], saved), next to any
///     other pause: no tick, and no rotation in flight is published. A
///     requested rotation and a repair of the slot go on, with their key
///     write, and the clock runs on. `Resume` ends it, and an external-change
///     pause with it, whose key is then written back: an overdue style
///     rotates at once, any other keeps its deadline. Only `Resume` ends the
///     user pause.
///   - `Reload` emits [`Effect::ReadConfig`]. [`Event::ConfigRead`] for it
///     is applied as [`Event::ConfigChanged`] and answered with `Reloaded`
///     right after the apply. One that no `Reload` waits for is ignored.
///   - `RefreshRemote` queues a fetch of every listed URL that has none
///     asked for, backoff or not.
///   - `Restore` emits [`Effect::RestoreKeys`] with the saved original
///     keys and user-pauses both styles. [`Event::KeysRestored`] for it is
///     answered with `Done`, or `Failed` with the write's error. The keys
///     are the user's until `Resume`, which writes the slot URIs again.
///   - `Status` replies with [`Model`]'s status report at once.
/// - [`Event::ScreenLock`]: records whether the screen is locked. With
///   `pause_when_locked` on, a locked screen pauses both styles
///   ([`Pause::ScreenLocked`], not saved), next to any other pause, as
///   `Pause` does: no tick, and no rotation in flight is published, while
///   requests and repairs go on and the clocks run on. Unlock ends it: an
///   overdue style rotates at once, any other keeps its deadline. A reload
///   that turns the setting on or off starts or ends the pause at once.
/// - [`Event::Shutdown`]: end the rotations in flight, fail every request
///   that waits, emit [`Effect::Arm`]`(None)` and [`Effect::Persist`].
///   Later events yield no effects, except a [`Reply::Failed`] for an
///   [`Event::Control`].
///
/// [`Effect::Changed`] follows each successful publish at once, and in uri
/// mode each key write, for every image the write names.
///
/// Derived effects follow the event's own, in this order:
/// - The first probe of every `Next` and `Previous` whose style is free, and
///   an [`Effect::Reply`] for every one that is over.
/// - [`Effect::Republish`] for the dirty slot on screen, once its refresh is
///   due and nothing else is in flight for its style. That uses the mark
///   up: one refresh per mark, whatever comes of it. While it is in flight
///   no rotation of that style starts. The refresh of a paused style waits
///   for the pause to end, because its key names something else.
/// - The rotations that are due.
/// - [`Effect::SetKeys`] for every key that does not hold what it must: the
///   slot URI when the slot file exists, or in uri mode the URI of the image
///   that passed its probe. So no key ever points at a missing file, and
///   the key of a style with nothing to show stays as it is. A key that
///   holds its URI is not written again, except to write it back after an
///   external change. No key of a paused style is written. All owed keys go
///   out in one write, and only while nothing is in flight for either
///   style: after the images they name, and together when both styles
///   change together.
///   `picture_options` goes with the first write: it comes from the config,
///   or is `Zoom` when the config has none and GNOME's value is `"none"`. An
///   [`Effect::Persist`] always precedes the write: the original keys are
///   saved before the first key write.
/// - [`Effect::Fetch`] for every URL whose fetch is due, oldest first, as
///   long as fewer than [`MAX_FETCHES_IN_FLIGHT`] are unanswered. Never two
///   for one URL. No rotation waits for a fetch, and readiness does not.
/// - [`Effect::CollectGarbage`], keeping every listed URL: once the cache
///   is loaded and a config is known, and then every [`GC_PERIOD`] while
///   any remote image is known.
/// - [`Effect::Arm`] whenever the earliest deadline differs from what is
///   armed: the next rotation of a style that waits for one, the next probe
///   of a truncated file, the refresh of a dirty slot, the write-back of a
///   key under `reassert-now`, the next fetch or the next garbage
///   collection. It lies after `now`.
/// - [`Effect::Ready`] exactly once, in the first result after which
///   nothing is in flight. When the config lists URLs, not before
///   [`Event::CacheLoaded`]: the cached images belong to the first apply.
/// - [`Effect::Report`] when the status differs from the last one reported,
///   nothing is in flight and [`Effect::Ready`] went out.
pub fn step(model: &mut Model, now: SystemTime, ev: Event) -> Vec<Effect> {
    let mut turn = Turn {
        model,
        now,
        effects: Vec::new(),
    };
    match turn.model.phase {
        Phase::Created => turn.await_start(ev),
        Phase::Starting | Phase::Running => turn.run(ev),
        Phase::Stopped => turn.after_stop(ev),
    }
    turn.effects
}

/// One call of [`step`]: the model, the time of the event and the effects
/// collected so far.
struct Turn<'a> {
    model: &'a mut Model,
    now: SystemTime,
    effects: Vec<Effect>,
}

impl Turn<'_> {
    fn await_start(&mut self, ev: Event) {
        match ev {
            Event::Started { gnome, slots } => self.start(gnome, slots),
            // Nothing ran yet, so there is nothing to save.
            Event::Shutdown => self.model.phase = Phase::Stopped,
            Event::Control { id, .. } => self.control_before_start(id),
            Event::ScreenLock(lock) => self.model.screen_lock = lock,
            Event::Fired(_)
            | Event::ConfigChanged(_)
            | Event::Scanned { .. }
            | Event::Fs { .. }
            | Event::Probed { .. }
            | Event::Published { .. }
            | Event::Republished { .. }
            | Event::SlotLost(_)
            | Event::CacheLoaded(_)
            | Event::Fetched { .. }
            | Event::ConfigRead { .. }
            | Event::KeysRestored { .. }
            | Event::Gnome(_) => {}
        }
    }

    fn run(&mut self, ev: Event) {
        self.absorb_clock_steps();
        self.end_probe_waits();
        match ev {
            Event::Fired(Fired::Deadline) => self.model.timer = Timer::Set(None),
            Event::Fired(Fired::ClockChanged) => self.model.timer = Timer::Stale,
            Event::Probed {
                style,
                item,
                verdict,
            } => self.on_probed(style, &item, verdict),
            Event::Published {
                style,
                item,
                result,
            } => self.on_published(style, item, result),
            Event::Gnome(GnomeEvent::StyleChanged(visible)) => self.switch_style(visible),
            Event::Gnome(GnomeEvent::KeysChanged(keys)) => self.on_keys_changed(keys),
            Event::Gnome(GnomeEvent::LocksChanged(locks)) => self.on_locks_changed(locks),
            Event::Gnome(GnomeEvent::ConnectivityChanged(level)) => self.on_connectivity(level),
            Event::CacheLoaded(blobs) => self.on_cache_loaded(blobs),
            Event::Fetched { url, outcome } => self.on_fetched(&url, outcome),
            Event::ConfigChanged(Ok(config)) => self.reload(config),
            // The running config stays. The interpreter logged what is wrong
            // with the file.
            Event::ConfigChanged(Err(_)) => {}
            Event::Control { id, command } => self.on_control(id, command),
            Event::ConfigRead { id, result } => self.on_config_read(id, result),
            Event::KeysRestored { id, result } => self.on_keys_restored(id, result),
            Event::Scanned { style, files } => {
                let delta = self.model.replace_catalog(style, files);
                self.follow_catalog(style, &delta);
            }
            Event::Fs { style, changes } => {
                let delta = self.model.apply_changes(style, changes);
                self.follow_catalog(style, &delta);
            }
            Event::Republished { style, result } => self.on_republished(style, result),
            Event::SlotLost(style) => self.on_slot_lost(style),
            Event::ScreenLock(lock) => self.model.screen_lock = lock,
            // Sent twice: the first one counts.
            Event::Started { .. } => {}
            Event::Shutdown => {
                self.stop();
                return;
            }
        }
        self.settle();
    }

    fn start(&mut self, gnome: GnomeSnapshot, slots: PerStyle<Option<FileId>>) {
        let picture_options = keys::picture_options_to_write(self.model.config(), &gnome.keys);
        self.model.phase = Phase::Starting;
        self.model.visible = gnome.visible;
        self.model.picture_options_owed = picture_options;
        self.watch_sources();
        self.adopt_keys(&gnome);
        self.model.original_keys.get_or_insert(gnome.keys);
        self.absorb_clock_steps();
        for style in Style::ALL {
            self.first_apply(style, slots[style]);
            self.model.follow_visibility(style, self.now);
        }
        self.plan_refresh();
        self.settle();
    }

    /// Asks for the scan and the watch of each style's sources. The first
    /// apply waits for neither.
    fn watch_sources(&mut self) {
        for style in Style::ALL {
            let sources = self.model.config().styles[style].sources.clone();
            self.effects.push(Effect::Watch { style, sources });
        }
    }

    fn stop(&mut self) {
        self.model.phase = Phase::Stopped;
        self.model.abandon_rotations();
        self.refuse_waiting_requests();
        self.effects.push(Effect::Arm(None));
        self.effects.push(Effect::Persist);
    }

    /// The clock-went-back rule, for both styles and for every short wait:
    /// no deadline stays further ahead than its whole delay.
    fn absorb_clock_steps(&mut self) {
        let now = self.now;
        for style in Style::ALL {
            let state = &mut self.model.styles[style];
            state.clock.absorb_clock_step(now);
            if let Some(Reassert::At(due)) = &mut state.key.reassert {
                *due = no_later_than(*due, now, REASSERT_DELAY);
            }
            state.refresh_due = state
                .refresh_due
                .map(|due| no_later_than(due, now, HIDDEN_REFRESH_DELAY));
        }
        for retry in self.model.probe_retries.values_mut() {
            retry.due = retry
                .due
                .map(|due| no_later_than(due, now, PROBE_RETRY_DELAY));
        }
        self.model.remote.absorb_clock_step(now);
    }

    /// Lets every truncated file whose wait is over be picked again.
    fn end_probe_waits(&mut self) {
        let released = self.model.release_probe_retries(self.now);
        self.follow_dropped_checks(released);
    }

    /// Follows the items whose check was dropped, per catalog. From here on
    /// each one is like a file that changed.
    fn follow_dropped_checks(&mut self, mut unchecked: PerStyle<Vec<ItemId>>) {
        for style in Style::ALL {
            let delta = CatalogDelta {
                added: Vec::new(),
                modified: mem::take(&mut unchecked[style]),
            };
            if !delta.modified.is_empty() {
                self.follow_catalog(style, &delta);
            }
        }
    }

    /// Start-up: makes `style`'s slot show its current item, or a first pick.
    fn first_apply(&mut self, style: Style, slot_on_disk: Option<FileId>) {
        let state = &mut self.model.styles[style];
        let is_our_slot = slot_on_disk.is_some() && slot_on_disk == state.slot;
        let shows_current = is_our_slot && state.current.is_some();
        state.slot = slot_on_disk;
        // In uri mode no image passed a probe in this run yet.
        if !shows_current || !self.model.has_wallpaper(style) {
            self.refill(style);
        }
    }

    /// Starts to fill `style`'s slot: with its current item when that can
    /// still be published, else with a first pick. With no image for it, the
    /// slot stays as it is.
    fn refill(&mut self, style: Style) {
        if self.model.awaits_config() {
            // It is filled once a config arrives. Until then it is not the
            // daemon's, whatever it holds.
            self.model.clear_wallpaper(style);
            return;
        }
        if self.model.styles[style].key.is_disabled() {
            return;
        }
        let refill = self
            .model
            .current_candidate(style)
            .or_else(|| self.model.pick(style, &[]));
        if let Some(candidate) = refill {
            // Whatever the file holds now, the slot is still to be filled.
            self.model.clear_wallpaper(style);
            self.probe(style, candidate, Vec::new());
        }
    }

    /// Someone removed `style`'s slot file: the desktop is blank until the
    /// file is back.
    fn on_slot_lost(&mut self, style: Style) {
        self.model.styles[style].slot = None;
        // A rotation in flight puts its image there.
        if !self.model.styles[style].is_busy() {
            self.refill(style);
        }
    }

    /// Follows a change to `changed`'s catalog. The items that are new or
    /// have new content join the shuffle rounds. A style whose image on
    /// screen has new content publishes it again. Then every slot that
    /// waited for an image gets one.
    fn follow_catalog(&mut self, changed: Style, delta: &CatalogDelta) {
        self.model.join_rounds(changed, delta);
        for style in Style::ALL {
            let on_screen = self.model.styles[style].current.as_ref();
            let on_screen_changed = self.model.pool(style) == Some(changed)
                && on_screen.is_some_and(|current| delta.modified.contains(current));
            if on_screen_changed {
                self.publish_again(style);
            }
            // No slot keeps the data in uri mode: the style needs another
            // image now.
            if self.model.names_a_gone_file(style) {
                self.model.clear_wallpaper(style);
            }
        }
        self.fill_empty_slots();
    }

    /// Publishes `style`'s current item again, because its file changed. The
    /// slot gets the new content on a new inode, which makes the shell load
    /// it. That is a repair, so it does not wait for a hidden style to be
    /// shown.
    fn publish_again(&mut self, style: Style) {
        let state = &self.model.styles[style];
        // A rotation in flight replaces the slot anyway.
        if state.is_busy() || state.key.is_disabled() {
            return;
        }
        if let Some(candidate) = self.model.current_candidate(style) {
            self.probe(style, candidate, Vec::new());
        }
    }

    /// Starts to fill every slot that shows no item of the daemon's, now
    /// that there may be an image for it.
    fn fill_empty_slots(&mut self) {
        for style in Style::ALL {
            let state = &self.model.styles[style];
            let shows_an_item = self.model.has_wallpaper(style) && state.current.is_some();
            if !shows_an_item && !state.is_busy() {
                self.refill(style);
            }
        }
    }

    fn switch_style(&mut self, visible: Style) {
        if visible == self.model.visible {
            return;
        }
        self.model.visible = visible;
        for style in Style::ALL {
            self.model.follow_visibility(style, self.now);
        }
        self.plan_refresh();
    }

    fn on_probed(&mut self, style: Style, item: &ItemId, verdict: Result<(), BadReason>) {
        let Some(rotation) = self.model.take_rotation(style, item, Stage::Probing) else {
            return;
        };
        let passed = verdict.is_ok();
        let lost_blob = match (&verdict, &rotation.candidate.version) {
            (Err(BadReason::Missing), Some(Version::Blob(remote))) => Some(remote.clone()),
            _ => None,
        };
        self.remember_verdict(&rotation.candidate, verdict);
        match self.model.candidate(style, item) {
            // The file changed under the probe: the verdict was about
            // content that is gone.
            Some(fresh) if rotation.candidate.is_outdated_by(&fresh) => {
                self.probe(style, fresh, rotation.failed);
            }
            Some(_) if passed => self.publish_if_allowed(style, rotation),
            // The probe failed, or the item left the catalog meanwhile.
            _ => self.drop_candidate(style, rotation),
        }
        // Someone emptied the cache. The rotation moved on already.
        if let Some(remote) = lost_blob {
            self.lose_blob(&remote);
        }
    }

    /// Records a probe verdict for the version of the file that was probed.
    /// A truncated file may get another probe; any other verdict ends its
    /// count of them.
    fn remember_verdict(&mut self, probed: &Candidate, verdict: Result<(), BadReason>) {
        let is_truncated = verdict == Err(BadReason::Truncated);
        if !self.model.record_verdict(probed, verdict) {
            return;
        }
        let earlier = self.model.probe_retries.remove(&probed.id);
        if is_truncated {
            self.grant_probe_retry(probed, earlier);
        }
    }

    /// Grants a truncated file its next probe, [`PROBE_RETRY_DELAY`] from
    /// now, unless that version of it had [`MAX_PROBE_RETRIES`] already.
    /// `earlier` is what the item was granted before.
    fn grant_probe_retry(&mut self, probed: &Candidate, earlier: Option<ProbeRetry>) {
        let due = self.now.checked_add(PROBE_RETRY_DELAY);
        let (Some(version), Some(_)) = (&probed.version, due) else {
            return;
        };
        let earlier = earlier.filter(|retry| retry.version == *version);
        let granted_before = earlier.as_ref().map_or(0, |retry| retry.granted);
        let retry = match earlier {
            // Both styles can probe one file at the same time. The second
            // verdict finds the file waiting, or out of probes: it is no
            // second probe spent, and no new one to grant.
            Some(spent) if spent.due.is_some() || spent.granted >= MAX_PROBE_RETRIES => spent,
            _ => ProbeRetry {
                version: version.clone(),
                granted: granted_before + 1,
                due,
            },
        };
        self.model.probe_retries.insert(probed.id.clone(), retry);
    }

    /// Publishes a candidate that passed its probe.
    ///
    /// A hidden slot must not get another image. When the style left the
    /// screen while the probe ran, the rotation ends here. Its clock stopped
    /// at zero, so the rotation starts again once the style is shown, with
    /// the same candidate when the order is random. A style that the user
    /// paused, or whose screen locked under `pause_when_locked`, while the
    /// probe ran keeps its image too, unless its slot is still to be filled.
    fn publish_if_allowed(&mut self, style: Style, probed: Rotation) {
        let is_the_same_image = self.model.shows(style, &probed.candidate.id);
        let is_held = self.model.styles[style].user_paused || self.model.is_held_by_lock();
        let is_held_by_pause = is_held && self.model.has_wallpaper(style);
        let may_rotate = self.model.is_running(style) && !is_held_by_pause;
        let may_change = may_rotate || self.model.is_requested(style);
        if is_the_same_image || may_change {
            self.publish(style, probed);
        } else {
            self.model.return_to_round(style, probed.into_taken());
        }
    }

    fn on_published(&mut self, style: Style, item: ItemId, result: Result<SlotWrite, String>) {
        let Some(rotation) = self.model.take_rotation(style, &item, Stage::Publishing) else {
            return;
        };
        match result {
            Ok(write) => {
                let changed_on_its_way = self
                    .model
                    .candidate(style, &item)
                    .is_some_and(|fresh| rotation.candidate.is_outdated_by(&fresh));
                self.model.return_to_round(style, rotation.failed);
                self.record_publish(style, item, write);
                if changed_on_its_way {
                    self.publish_again(style);
                } else if !self.model.has_wallpaper(style) {
                    // A reload to uri mode overtook the slot write: the key
                    // is to name the image's own file.
                    self.refill(style);
                }
            }
            // The interpreter logged the error text.
            Err(_) => self.drop_candidate(style, rotation),
        }
    }

    fn record_publish(&mut self, style: Style, item: ItemId, write: SlotWrite) {
        let state = &mut self.model.styles[style];
        state.slot = Some(write.slot);
        // The shell watches only the slot it shows. Of any other it may keep
        // a texture from before this write.
        if style != self.model.visible {
            state.dirty_while_hidden = true;
        }
        self.effects.push(Effect::Changed {
            style,
            item: item.clone(),
        });
        self.record_show(style, item);
    }

    /// Records that `style` shows `item` now.
    fn record_show(&mut self, style: Style, item: ItemId) {
        let state = &mut self.model.styles[style];
        let image_changed = state.current.as_ref() != Some(&item);
        // A slot refilled with the image it had keeps its interval.
        if image_changed {
            state.clock.record_change(self.now);
            let replaced = state.current.clone();
            if let Some(replaced) = replaced.filter(|_| !self.model.steps_back(style)) {
                self.model.styles[style].history.push(replaced);
            }
            self.model.set_current(style, item);
        }
        // The clock ran while the slot was to be filled, on screen or not.
        self.model.follow_visibility(style, self.now);
        self.persist();
    }

    /// Starts or continues a rotation: `failed` are the candidates it gave up
    /// on before this one.
    fn probe(&mut self, style: Style, candidate: Candidate, failed: Vec<ItemId>) {
        self.effects.push(Effect::Probe {
            style,
            item: candidate.id.clone(),
            path: candidate.path.clone(),
            max_pixels: self.model.config().general.max_pixels,
        });
        self.model.styles[style].rotation = Some(Rotation {
            candidate,
            stage: Stage::Probing,
            failed,
        });
    }

    /// Puts a candidate that passed its probe on screen: into the slot, or
    /// in uri mode into the key.
    fn publish(&mut self, style: Style, probed: Rotation) {
        match self.model.config().general.apply_mode {
            ApplyMode::Slots => self.publish_to_slot(style, probed),
            ApplyMode::Uri => self.show_by_uri(style, probed),
        }
    }

    /// Uri mode: `style`'s key is to name the candidate's own file. No slot
    /// file is touched, and the key is written when the step settles.
    fn show_by_uri(&mut self, style: Style, probed: Rotation) {
        // Only a relative path has no URI, and only a state file that
        // someone edited names one.
        let Ok(uri) = Url::from_file_path(&probed.candidate.path) else {
            return self.drop_candidate(style, probed);
        };
        self.model.return_to_round(style, probed.failed);
        self.model.styles[style].shown_uri = Some(uri.into());
        self.record_show(style, probed.candidate.id);
    }

    fn publish_to_slot(&mut self, style: Style, probed: Rotation) {
        self.effects.push(Effect::Publish {
            style,
            item: probed.candidate.id.clone(),
            path: probed.candidate.path.clone(),
            link: self.model.config().general.link,
        });
        self.model.styles[style].rotation = Some(Rotation {
            stage: Stage::Publishing,
            ..probed
        });
    }

    /// Gives up on a rotation's candidate. A rotation that only publishes
    /// again what its slot shows ends here: the slot keeps its image until
    /// the next tick. Any other rotation moves on.
    fn drop_candidate(&mut self, style: Style, rotation: Rotation) {
        if self.model.shows(style, &rotation.candidate.id) {
            return;
        }
        if self.model.steps_back(style) {
            self.step_back_further(style);
        } else {
            self.retry(style, rotation);
        }
    }

    /// Moves a rotation whose candidate failed on to its next candidate.
    fn retry(&mut self, style: Style, rotation: Rotation) {
        let failed = rotation.into_taken();
        let next = if failed.len() < MAX_PUBLISH_ATTEMPTS as usize {
            self.model.pick(style, &failed)
        } else {
            None
        };
        self.try_candidate(style, next, failed);
    }

    fn try_candidate(&mut self, style: Style, candidate: Option<Candidate>, failed: Vec<ItemId>) {
        match candidate {
            Some(candidate) => self.probe(style, candidate, failed),
            None => self.give_up(style, failed),
        }
    }

    /// Ends a rotation that found nothing to publish. The slot is kept, and
    /// a fresh interval stops the style from trying again on every event.
    ///
    /// A slot that was lost while the rotation ran is still to be filled:
    /// with the image it showed, unless the rotation tried that one too.
    fn give_up(&mut self, style: Style, failed: Vec<ItemId>) {
        self.model.restart_clock(style, self.now);
        self.persist();
        let refill = self
            .model
            .current_candidate(style)
            .filter(|current| !self.model.has_wallpaper(style) && !failed.contains(&current.id));
        match refill {
            Some(current) => self.probe(style, current, failed),
            None => self.model.return_to_round(style, failed),
        }
    }

    fn persist(&mut self) {
        if !self.effects.contains(&Effect::Persist) {
            self.effects.push(Effect::Persist);
        }
    }

    /// Brings everything that follows from the state up to date: rotations
    /// that are due, the key write, the timer, readiness and the status.
    fn settle(&mut self) {
        self.serve_requests();
        self.refresh_stale_slots();
        self.rotate_due_styles();
        self.end_reassert_waits();
        self.write_owed_keys();
        self.start_due_fetches();
        self.collect_due_garbage();
        self.arm_timer();
        self.signal_ready_once();
        self.report_status();
    }

    fn rotate_due_styles(&mut self) {
        for style in Style::ALL {
            if self.model.is_due(style, self.now) {
                self.model.reassert_at_tick();
                let next = self.model.pick(style, &[]);
                self.try_candidate(style, next, Vec::new());
            }
        }
    }

    fn arm_timer(&mut self) {
        let deadline = self.model.next_deadline();
        if self.model.timer != Timer::Set(deadline) {
            self.model.timer = Timer::Set(deadline);
            self.effects.push(Effect::Arm(deadline));
        }
    }

    /// The first apply includes the cached remote images, so `Ready` waits
    /// for the cache when the config lists URLs.
    fn signal_ready_once(&mut self) {
        if self.model.phase != Phase::Starting || self.model.is_busy() {
            return;
        }
        let awaits_cache = !self.model.remote.is_loaded() && !self.model.listed_urls().is_empty();
        if !awaits_cache {
            self.model.phase = Phase::Running;
            self.effects.push(Effect::Ready);
        }
    }

    /// No status goes out before `Ready`, so that `Ready` comes first.
    fn report_status(&mut self) {
        if self.model.is_busy() || self.model.phase == Phase::Starting {
            return;
        }
        let status = self.model.status();
        if self.model.reported.as_ref() != Some(&status) {
            self.model.reported = Some(status.clone());
            self.effects.push(Effect::Report(status));
        }
    }
}

/// `due`, but no further ahead of `now` than `delay`: the wall clock may
/// have gone back since the wait began.
pub(crate) fn no_later_than(due: SystemTime, now: SystemTime, delay: Duration) -> SystemTime {
    now.checked_add(delay).map_or(due, |latest| due.min(latest))
}
