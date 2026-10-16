//! The form of an external reference (KAIROS-T-0508, COLLIERY-I-0630): an
//! item links to a URI with a label. One function has the rule of the URI
//! and one the rule of the label, so the server, the CLI and the web
//! cannot go apart.
//!
//! The rule is narrow on purpose. The web page renders the URI as the
//! `href` of a link, so a scheme that a browser runs as code
//! (`javascript:`, `data:`) must not pass. The list of the schemes that
//! pass is the whole rule: each other scheme is refused, and the refusal
//! names it.

/// The name of the relationship, as the API and the tools show it.
pub const RELATIONSHIP: &str = "external";

/// The schemes that a URI can have, in the order the refusal names them.
pub const SCHEMES: &[&str] = &["http", "https", "mailto"];

/// The longest URI that passes, in bytes.
pub const MAX_URI_LEN: usize = 2048;

/// The longest label that passes, in characters.
pub const MAX_LABEL_LEN: usize = 200;

/// Why a URI or a label does not pass.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExternalRefError {
    /// The URI is empty, or only whitespace.
    #[error("The uri is empty. Send a URI with the scheme http, https or mailto.")]
    EmptyUri,
    /// The URI has a character that a URI cannot have.
    #[error("The uri has whitespace or a control character. Encode it, or remove it.")]
    BadCharacter,
    /// The URI has no `scheme:` prefix.
    #[error("The uri {uri:?} has no scheme. Send a URI with the scheme http, https or mailto.")]
    NoScheme { uri: String },
    /// The scheme is not one of [`SCHEMES`].
    #[error("Kairos does not permit the scheme {scheme:?}. The schemes are: http, https, mailto.")]
    Scheme { scheme: String },
    /// The URI has a scheme and nothing after it.
    #[error("The uri {uri:?} has no address after the scheme.")]
    NoAddress { uri: String },
    /// The URI is longer than [`MAX_URI_LEN`] bytes.
    #[error("The uri is longer than {MAX_URI_LEN} bytes.")]
    UriTooLong,
    /// The label is longer than [`MAX_LABEL_LEN`] characters.
    #[error("The label is longer than {MAX_LABEL_LEN} characters.")]
    LabelTooLong,
}

/// The URI as Kairos stores it, when `uri` passes the rule: trimmed, with
/// the scheme in lower case. The rest of the URI is kept as sent: the
/// path of an HTTP URI and the address of a `mailto:` URI can be case
/// sensitive.
///
/// The rule (RFC 3986 section 3.1 for the form of the scheme):
///
/// 1. not empty after a trim ([`ExternalRefError::EmptyUri`]);
/// 2. no whitespace and no control character ([`ExternalRefError::BadCharacter`]);
/// 3. at most [`MAX_URI_LEN`] bytes ([`ExternalRefError::UriTooLong`]);
/// 4. a scheme before the first `:` ([`ExternalRefError::NoScheme`]), one
///    of [`SCHEMES`] ([`ExternalRefError::Scheme`]);
/// 5. something after the `:` ([`ExternalRefError::NoAddress`]).
pub fn parse_uri(uri: &str) -> Result<String, ExternalRefError> {
    let uri = uri.trim();
    if uri.is_empty() {
        return Err(ExternalRefError::EmptyUri);
    }
    if uri.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ExternalRefError::BadCharacter);
    }
    if uri.len() > MAX_URI_LEN {
        return Err(ExternalRefError::UriTooLong);
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return Err(ExternalRefError::NoScheme {
            uri: uri.to_string(),
        });
    };
    if !is_scheme_form(scheme) {
        return Err(ExternalRefError::NoScheme {
            uri: uri.to_string(),
        });
    }
    let scheme = scheme.to_ascii_lowercase();
    if !SCHEMES.contains(&scheme.as_str()) {
        return Err(ExternalRefError::Scheme { scheme });
    }
    if rest.is_empty() || rest == "//" {
        return Err(ExternalRefError::NoAddress {
            uri: uri.to_string(),
        });
    }
    Ok(format!("{scheme}:{rest}"))
}

/// RFC 3986 section 3.1: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
fn is_scheme_form(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The label as Kairos stores it: trimmed, and `None` when it is empty.
/// A label longer than [`MAX_LABEL_LEN`] characters is
/// [`ExternalRefError::LabelTooLong`].
pub fn parse_label(label: Option<&str>) -> Result<Option<String>, ExternalRefError> {
    let Some(label) = label.map(str::trim).filter(|label| !label.is_empty()) else {
        return Ok(None);
    };
    if label.chars().count() > MAX_LABEL_LEN {
        return Err(ExternalRefError::LabelTooLong);
    }
    Ok(Some(label.to_string()))
}

/// The text that shows a reference: the label when there is one, the URI
/// when there is none. The web page uses it as the text of the link, and
/// `get_item` in its line.
pub fn display_text(uri: &str, label: Option<&str>) -> String {
    match label {
        Some(label) if !label.is_empty() => label.to_string(),
        _ => uri.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_https_and_mailto_pass_with_the_scheme_in_lower_case() {
        assert_eq!(
            parse_uri("https://example.com/rfc/42").as_deref(),
            Ok("https://example.com/rfc/42")
        );
        assert_eq!(
            parse_uri("  HTTP://Example.com/A?b=C#d  ").as_deref(),
            Ok("http://Example.com/A?b=C#d")
        );
        assert_eq!(
            parse_uri("mailto:robin@example.com").as_deref(),
            Ok("mailto:robin@example.com")
        );
    }

    #[test]
    fn each_other_scheme_is_refused_and_named() {
        assert_eq!(
            parse_uri("javascript:alert(1)"),
            Err(ExternalRefError::Scheme {
                scheme: "javascript".into()
            })
        );
        assert_eq!(
            parse_uri("JavaScript:alert(1)"),
            Err(ExternalRefError::Scheme {
                scheme: "javascript".into()
            })
        );
        assert_eq!(
            parse_uri("data:text/html,x"),
            Err(ExternalRefError::Scheme {
                scheme: "data".into()
            })
        );
        assert_eq!(
            parse_uri("ftp://example.com/x"),
            Err(ExternalRefError::Scheme {
                scheme: "ftp".into()
            })
        );
        assert_eq!(
            parse_uri("file:///etc/passwd"),
            Err(ExternalRefError::Scheme {
                scheme: "file".into()
            })
        );
        let text = parse_uri("javascript:alert(1)").unwrap_err().to_string();
        assert!(text.contains("\"javascript\""), "{text}");
        assert!(text.contains("http, https, mailto"), "{text}");
    }

    #[test]
    fn a_uri_with_no_scheme_or_no_address_is_refused() {
        assert_eq!(parse_uri(""), Err(ExternalRefError::EmptyUri));
        assert_eq!(parse_uri("   "), Err(ExternalRefError::EmptyUri));
        assert_eq!(
            parse_uri("example.com/x"),
            Err(ExternalRefError::NoScheme {
                uri: "example.com/x".into()
            })
        );
        // A port is not a scheme: the part before `:` must have the form
        // of a scheme. `example.com` has a dot, which a scheme can have,
        // so RFC 3986 reads it as a scheme; `1example` cannot be one.
        assert_eq!(
            parse_uri("1example:x"),
            Err(ExternalRefError::NoScheme {
                uri: "1example:x".into()
            })
        );
        assert_eq!(
            parse_uri("https:"),
            Err(ExternalRefError::NoAddress {
                uri: "https:".into()
            })
        );
        assert_eq!(
            parse_uri("https://"),
            Err(ExternalRefError::NoAddress {
                uri: "https://".into()
            })
        );
    }

    #[test]
    fn whitespace_inside_and_a_long_uri_are_refused() {
        assert_eq!(
            parse_uri("https://example.com/a b"),
            Err(ExternalRefError::BadCharacter)
        );
        assert_eq!(
            parse_uri("https://example.com/a\nb"),
            Err(ExternalRefError::BadCharacter)
        );
        let long = format!("https://example.com/{}", "a".repeat(MAX_URI_LEN));
        assert_eq!(parse_uri(&long), Err(ExternalRefError::UriTooLong));
    }

    #[test]
    fn the_label_is_trimmed_and_an_empty_label_is_none() {
        assert_eq!(parse_label(None), Ok(None));
        assert_eq!(parse_label(Some("  ")), Ok(None));
        assert_eq!(
            parse_label(Some("  RFC: zen as the policy engine ")),
            Ok(Some("RFC: zen as the policy engine".into()))
        );
        assert_eq!(
            parse_label(Some(&"x".repeat(MAX_LABEL_LEN + 1))),
            Err(ExternalRefError::LabelTooLong)
        );
        assert_eq!(
            parse_label(Some(&"é".repeat(MAX_LABEL_LEN))).map(|l| l.map(|l| l.chars().count())),
            Ok(Some(MAX_LABEL_LEN))
        );
    }

    #[test]
    fn the_display_text_is_the_label_or_the_uri() {
        assert_eq!(
            display_text("https://example.com/x", Some("RFC 42")),
            "RFC 42"
        );
        assert_eq!(
            display_text("https://example.com/x", None),
            "https://example.com/x"
        );
        assert_eq!(
            display_text("https://example.com/x", Some("")),
            "https://example.com/x"
        );
    }
}
