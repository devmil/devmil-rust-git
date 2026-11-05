//! Installed Git execution without a shell, with bounded and redacted activity.
//!
//! The repository model owns loading, watching, events, batching and caches.
//! Applications retain UI state, authorization and recovery policy.

#![doc = include_str!("../README.md")]

pub mod activity;
pub mod archive;
pub mod auth;
pub mod cache;
pub mod cancellation;
pub mod clone;
pub mod commands;
pub mod diagnosis;
pub mod diff;
pub mod environment;
pub mod error;
pub mod filesystem;
pub mod git_locks;
pub mod history;
#[cfg(feature = "hosting")]
pub mod hosting;
pub mod ignore;
pub mod index_lock;
pub mod index_transaction;
pub mod integrations;
pub mod lfs;
pub mod loading;
pub mod merge;
pub mod model;
pub mod native_bytes;
pub mod probes;
pub mod process;
pub mod rebase;
pub mod refs;
pub mod repository;
pub mod sequence_editor;
pub mod status;
pub mod status_acceleration;
pub mod submodule;
pub mod transfer;
pub mod types;
pub mod version;
pub mod watch;

pub use model::{
    IndexRefreshPolicy, ReadTicket, Repository, RepositoryError, RepositoryErrorKind,
    RepositoryEvent, RepositoryOptions, RepositoryPhase, RepositorySnapshot,
    RepositorySubscription,
};
