//! The GitLab GraphQL data layer: the only part of `mrq` that touches the network.
//!
//! Depends on `config` for the token and filter definitions, and on nothing else in
//! the crate — it hands `app` a snapshot of domain types and never renders or reads
//! terminal state.
//!
//! Planned contents:
//!
//! - `model`   — the domain types, deliberately not the GraphQL shape
//! - `client`  — reqwest transport, auth headers, request/response envelopes
//! - `query`   — the `MrFields` fragment and the per-scope query builders
//! - `page`    — cursor pagination bounded by `max_results`
//! - `error`   — HTTP/GraphQL error classification and the retry policy

pub mod client;
pub mod error;
pub mod model;
pub mod probe;
pub mod query;
pub mod wire;

pub mod fetch;

/// The whole fetch path against recorded responses.
#[cfg(test)]
mod integration;
