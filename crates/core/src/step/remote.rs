//! Remote images: the cache at start, the fetches and their answers. No
//! rotation waits for any of it.

use url::Url;

use super::Turn;
use crate::catalog::RemoteBlob;
use crate::event::{Connectivity, Effect, FetchOutcome};
use crate::style::Style;

impl Turn<'_> {
    /// The cached images join the catalogs, and the first fetches are
    /// planned. A second report changes nothing.
    pub(super) fn on_cache_loaded(&mut self, blobs: Vec<RemoteBlob>) {
        let listed = self.model.listed_urls();
        let policy = self.model.config().general.remote_refresh;
        if self.model.remote.load(blobs, &listed, policy) {
            self.follow_remote_items();
        }
    }

    /// A new blob adds or updates its URL's items. Any other answer leaves
    /// the catalogs alone: a cached blob stays, whatever came of the fetch.
    pub(super) fn on_fetched(&mut self, url: &Url, outcome: FetchOutcome) {
        if self.model.remote.answer(url, outcome, self.now) {
            self.follow_remote_items();
        }
        // A URL that turned out gone no longer holds its style back from
        // the other style's images.
        self.fill_empty_slots();
    }

    pub(super) fn on_connectivity(&mut self, level: Connectivity) {
        if level == Connectivity::Full {
            self.model.remote.retry_failed();
        }
    }

    /// A reload: the URLs that it adds are fetched, and the ones that no
    /// style lists any more leave the catalogs and have their fetches
    /// dropped.
    pub(super) fn reload_remotes(&mut self) {
        let listed = self.model.listed_urls();
        let policy = self.model.config().general.remote_refresh;
        for url in self.model.remote.follow_config(&listed, policy) {
            self.effects.push(Effect::DropRemote(url));
        }
        self.follow_remote_items();
    }

    /// `remote`'s blob is missing: its item leaves the catalogs until a
    /// fetch brings it back. A slot keeps its own copy of the data.
    pub(super) fn lose_blob(&mut self, remote: &RemoteBlob) {
        if self.model.remote.lose_blob(remote) {
            self.follow_remote_items();
        }
    }

    /// Brings each catalog's remote items in line with the config and the
    /// cache, and follows what changed.
    fn follow_remote_items(&mut self) {
        let deltas = self.model.sync_remote_items();
        for style in Style::ALL {
            self.follow_catalog(style, &deltas[style]);
        }
    }

    /// Asks for every fetch that is due, as far as there is room in flight.
    pub(super) fn start_due_fetches(&mut self) {
        let policy = self.model.config().general.remote_refresh;
        for url in self.model.remote.start_due_fetches(self.now, policy) {
            self.effects.push(Effect::Fetch(url));
        }
    }

    /// Asks for a garbage collection when one is due. A daemon without a
    /// config file does not know which URLs to keep.
    pub(super) fn collect_due_garbage(&mut self) {
        let due = self.model.remote.garbage_due();
        if self.model.awaits_config() || due.is_none_or(|due| self.now < due) {
            return;
        }
        let keep = self.model.listed_urls();
        self.model.remote.record_garbage_collection(self.now, &keep);
        self.effects.push(Effect::CollectGarbage { keep });
    }
}
