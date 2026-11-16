use ocmini_core::types::{FailureKind, OperationError};
#[derive(Debug)]
pub struct HttpError {
    pub kind: FailureKind,
    pub message: String,
}
impl HttpError {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(FailureKind::Invalid, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(FailureKind::NotFound, message)
    }
}
impl From<OperationError> for HttpError {
    fn from(e: OperationError) -> Self {
        Self::new(e.kind, e.message)
    }
}
impl From<String> for HttpError {
    fn from(s: String) -> Self {
        Self::new(FailureKind::Error, s)
    }
}
impl From<&str> for HttpError {
    fn from(s: &str) -> Self {
        Self::from(s.to_owned())
    }
}
impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
