//! Configuration: XDG discovery, the TOML schema, token resolution and the keymap.
//!
//! This module is a leaf — it reads the environment and the filesystem and
//! hands back a validated `Config`. It must not depend on `app`, `ui`, `gitlab` or
//! `term`, so that configuration errors can be reported before the terminal is touched.
//!
//! Planned contents:
//!
//! - `paths`    — XDG resolution for the config, cache and state directories
//! - `schema`   — the serde types mirroring the TOML shape, parsed with `deny_unknown_fields`
//! - `token`    — the four-source PAT resolution chain and the redacting wrapper
//! - `keymap`   — key-spec grammar, default bindings, merge and conflict detection
//! - `validate` — semantic rules: interval floor, clamping, scope/argument compatibility
//! - `init`     — the shipped commented default file that `mrq init-config` writes

pub mod init;
pub mod keymap;
pub mod load;
pub mod paths;
pub mod schema;
pub mod token;
pub mod validate;

/// The config layer driven end to end from TOML text and a synthetic environment, as
/// opposed to the per-module tests that build values in code.
#[cfg(test)]
mod suite;
