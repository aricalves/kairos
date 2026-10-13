//! Policy documents.

mod document;

pub use document::{MAX_RULES, PolicyDocument, PolicyError, parse_document, validate};
