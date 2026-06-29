//! Framework-agnostic engine for the spotlight launcher.
//!
//! This crate knows nothing about GPUI or macOS. It defines the result model,
//! the [`Extension`] contract, a [`Registry`] that dispatches queries across
//! extensions, and a small fuzzy-matching helper. The UI and platform layers
//! build on top of these types.

mod extension;
mod item;
mod query;
mod registry;

pub mod fuzzy;

pub use extension::Extension;
pub use item::{Action, Icon, ResultItem};
pub use query::Query;
pub use registry::Registry;
