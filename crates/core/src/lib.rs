//! Pure core of gpaperd: config, catalog, ordering, scheduling and the reducer.
//!
//! No I/O, no async, no clock and no hidden randomness. Time is an argument of
//! [`step::step`] and the RNG is a seeded field of [`model::Model`], so a
//! sequence of events always replays to the same effects.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod config;
pub mod control;
pub mod event;
mod key;
pub mod model;
pub mod ordering;
mod remote;
pub mod schedule;
pub mod step;
pub mod style;
