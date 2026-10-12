//! Origin-host, read-only Claude saved conversation adapter.
pub mod binding;
mod claude;
mod index;
pub mod model;
pub use index::ConversationReader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryError {
    pub code: String,
    pub message: String,
}
impl HistoryError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl From<std::io::Error> for HistoryError {
    fn from(error: std::io::Error) -> Self {
        let code = match error.kind() {
            std::io::ErrorKind::NotFound => "source_missing",
            std::io::ErrorKind::PermissionDenied => "access_denied",
            _ => "source_unavailable",
        };
        Self::new(code, "saved conversation source cannot be read")
    }
}
#[cfg(test)]
mod tests;
