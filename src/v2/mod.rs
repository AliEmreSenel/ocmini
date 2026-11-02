//! OpenCode 2 wire contracts around the existing runtime and storage.
pub(crate) mod config;
mod events;
pub(crate) mod http;
pub(crate) mod wire;
pub use events::Events;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Input {
    #[serde(default = "steer")]
    pub delivery: String,
    #[serde(default = "yes")]
    pub resume: bool,
    #[serde(default = "user")]
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}
fn steer() -> String {
    "steer".into()
}
fn user() -> String {
    "user".into()
}
fn yes() -> bool {
    true
}
impl Default for Input {
    fn default() -> Self {
        Self {
            delivery: steer(),
            resume: true,
            kind: user(),
            payload: serde_json::json!({}),
        }
    }
}
