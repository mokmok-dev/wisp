/// Errors produced when parsing a [`crate::SourceLabel`] from its stable string form.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unrecognized source label: {0}")]
pub struct SourceLabelError(pub String);
