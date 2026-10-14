//! Policy documents.

mod document;
mod macros;

pub use document::{MAX_RULES, PolicyDocument, PolicyError, parse_document, validate};
