pub mod attachments;
pub mod auth;
pub mod blocking;
pub mod bus;
pub mod client;
pub mod commands;
pub mod config;
pub mod engine;
pub mod engine_impl;
pub mod events;
pub mod hooks;
pub mod http;
pub mod inventory;
pub mod mcp;
pub mod messages;
pub mod permission;
pub mod process;
pub mod provider;
pub mod sessions;
pub mod store;
pub mod title;
pub mod types;
pub mod usage_db;
pub mod v2;
pub extern crate ocmini_tools as tools;
pub const OPENCODE_COMPAT_VERSION: &str = concat!("2.2.5-ocmini.", env!("CARGO_PKG_VERSION"));

#[cfg(test)]
pub fn test_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)
}

#[cfg(feature = "sandbox")]
pub mod sandbox;
mod tool_host;
