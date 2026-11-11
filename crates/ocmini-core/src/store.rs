//! Backend-independent durable session/history handle.
mod backend;
mod shared;
pub mod sqlite;
use crate::types::OperationError;
#[cfg(test)]
use crate::types::Session;
pub use backend::{
    AdmissionReservation, HistoryBackend, HookDeliveryBudget, HookDeliveryRecord, InterruptedTurn,
    OutboxIntent, RecoveredTurn, SessionScope,
};
pub use shared::PreparedFork;
use std::path::Path;

/// Application backend selected at build time. SQLite is the shipped backend.
pub type SelectedBackend = sqlite::SqliteBackend;

/// All history persistence uses this statically dispatched handle. Shared
/// lifecycle policies run in core and coordinated writes commit in the backend.
pub struct Store<B: HistoryBackend = SelectedBackend> {
    backend: B,
}
impl Store {
    pub fn open(path: &Path, instance: &str) -> Result<Self, OperationError> {
        SelectedBackend::open(path, instance).map(Self::from_backend)
    }
    /// Read-only diagnostics, without migration or recovery.
    pub fn inspect_hook_deliveries(
        path: &Path,
        directory: &str,
    ) -> Result<Vec<serde_json::Value>, OperationError> {
        SelectedBackend::inspect_hook_deliveries(path, directory)
    }
}
impl<B: HistoryBackend> Store<B> {
    pub fn from_backend(backend: B) -> Self {
        Self { backend }
    }
    pub fn backend(&self) -> &B {
        &self.backend
    }
    #[cfg(test)]
    pub fn create_session_owned(&self, title: &str, dir: &str) -> Session {
        let session = Session::new(title, dir);
        self.create_session(&session).unwrap();
        session
    }
}

#[cfg(test)]
mod tests;
