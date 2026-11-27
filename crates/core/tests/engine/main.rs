//! Tests of the pure engine: catalog, ordering, scheduling and the reducer.
//!
//! One test crate, so the fixtures in `support` are shared without dead-code
//! warnings. The reducer has one module per topic, one behaviour per test.

mod ban;
mod catalog;
mod control;
mod external_changes;
mod favorite;
mod files;
mod hidden_refresh;
mod keys;
mod lifecycle;
mod locked_keys;
mod long_intervals;
mod lost_slot;
mod ordering;
mod properties;
mod reload;
mod remote;
mod rotation;
mod schedule;
mod screen_lock;
mod set;
mod sparse_styles;
mod startup;
mod support;
mod switching;
mod timer;
mod uri_mode;
mod validation;
