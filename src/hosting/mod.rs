//! Provider-neutral hosting connections, repository identities, and API records.

pub mod binding;
pub mod cli;
pub mod http;
pub mod model;
pub mod oauth;
pub mod parsers;
pub mod providers;
pub mod rest;
pub mod review;

pub use model::*;

#[cfg(test)]
mod tests;
