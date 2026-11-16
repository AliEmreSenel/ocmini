//! Linux permission jail and destination mediation. This crate contains no tools.
#[cfg(target_os = "linux")]
pub mod broker;
pub mod config;
#[cfg(target_os = "linux")]
pub mod jail;
pub mod wire;
pub const REFUSAL: i32 = 78;
pub fn refusal(error: impl std::fmt::Display) -> String {
    format!("Sandbox startup refused: {error}")
}
