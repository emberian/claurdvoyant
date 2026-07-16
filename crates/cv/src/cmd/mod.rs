//! One module per command family — see `main.rs` for the clap definitions and dispatch.

pub(crate) mod browse;
pub(crate) mod compose;
pub(crate) mod config;
pub(crate) mod convert;
pub(crate) mod doctor;
pub(crate) mod live;
pub(crate) mod pack;
pub(crate) mod provenance;
pub(crate) mod query;
pub(crate) mod search;
pub(crate) mod share;
pub(crate) mod task;
pub(crate) mod view;
pub(crate) mod workflow;
