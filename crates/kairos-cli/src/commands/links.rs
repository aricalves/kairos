//! `kairos link` and `kairos unlink` (KAIROS-T-0508, COLLIERY-I-0630):
//! the relationship `external`, from an item to a URI with a label.
//!
//! `kairos link <SHORT_CODE> external <URI> [--label TEXT]` and
//! `kairos unlink <SHORT_CODE> external <URI>`. The relationship is a
//! positional argument, so a later relationship with a target that is
//! not an item (as `impacts` of `kairos repos link`) is a new value and
//! no new command.
//!
//! The server applies the rule of the URI (the schemes `http`, `https`
//! and `mailto`) and the edit rule of the item. The CLI sends the text as
//! given.

use clap::{Args, ValueEnum};
use kairos_client::types_meta::ExternalRef;

use crate::commands::repos::family_of;
use crate::context::{Common, client, print_json};
use crate::error::CliError;

/// The relationships that `kairos link` can make. A relationship between
/// two items is `<noun> ...` work of the API (`POST /api/relationships`);
/// `impacts` is `kairos repos link`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LinkRelationship {
    /// A URI with a label: an RFC, a design page, an issue that Kairos
    /// does not hold
    External,
}

/// Link an item to a URI with a label (the relationship `external`)
#[derive(Debug, Args)]
#[command(
    long_about = "Link an item to a URI with a label: the relationship `external`.\n\n\
                  The scheme of the URI is http, https or mailto. The server refuses \
                  each other scheme and names it. You must be able to edit the item."
)]
pub struct LinkArgs {
    /// Short code of the item (any type)
    pub short_code: String,
    /// The relationship: `external`
    #[arg(value_enum)]
    pub relationship: LinkRelationship,
    /// The URI (http, https or mailto)
    pub target: String,
    /// The label of the reference, at most 200 characters
    #[arg(long)]
    pub label: Option<String>,
    #[command(flatten)]
    pub common: Common,
}

/// Remove the external reference of an item with a URI
#[derive(Debug, Args)]
pub struct UnlinkArgs {
    /// Short code of the item
    pub short_code: String,
    /// The relationship: `external`
    #[arg(value_enum)]
    pub relationship: LinkRelationship,
    /// The URI of the reference
    pub target: String,
    #[command(flatten)]
    pub common: Common,
}

/// The text of one reference: `label <uri>` or `<uri>`.
pub(crate) fn reference_text(reference: &ExternalRef) -> String {
    match reference.label.as_deref().filter(|label| !label.is_empty()) {
        Some(label) => format!("{label} <{}>", reference.uri),
        None => format!("<{}>", reference.uri),
    }
}

/// The URI as the server stores it, for the comparison of `unlink`:
/// trimmed, with the scheme in lower case. The rest of the rule is the
/// server's; a URI that does not pass it is in no list.
pub(crate) fn stored_form(uri: &str) -> String {
    let uri = uri.trim();
    match uri.split_once(':') {
        Some((scheme, rest)) => format!("{}:{rest}", scheme.to_ascii_lowercase()),
        None => uri.to_string(),
    }
}

impl LinkArgs {
    pub async fn run(self) -> Result<(), CliError> {
        let LinkRelationship::External = self.relationship;
        let kind = family_of(&self.short_code)?;
        let client = client(&self.common)?;
        let made = client
            .add_external_ref(kind, &self.short_code, &self.target, self.label.as_deref())
            .await?;
        if self.common.json {
            return print_json(&made);
        }
        println!(
            "Kairos made the link: {} -[external]-> {}.",
            self.short_code,
            reference_text(&made)
        );
        Ok(())
    }
}

impl UnlinkArgs {
    pub async fn run(self) -> Result<(), CliError> {
        let LinkRelationship::External = self.relationship;
        let kind = family_of(&self.short_code)?;
        let client = client(&self.common)?;
        let wanted = stored_form(&self.target);
        let listed = client.item_external_refs(kind, &self.short_code).await?;
        let Some(found) = listed
            .external
            .into_iter()
            .find(|reference| reference.uri == wanted)
        else {
            return Err(CliError::Failure(format!(
                "{} has no external reference {wanted}. `kairos {} get {}` shows its references.",
                self.short_code, kind, self.short_code
            )));
        };
        let removed = client
            .remove_external_ref(kind, &self.short_code, &found.id)
            .await?;
        if self.common.json {
            return print_json(&removed);
        }
        println!(
            "Kairos removed the link: {} -[external]-> {}.",
            self.short_code,
            reference_text(&found)
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(uri: &str, label: Option<&str>) -> ExternalRef {
        ExternalRef {
            id: "r-1".into(),
            relationship: "external".into(),
            uri: uri.into(),
            label: label.map(str::to_string),
            created_by: "u-1".into(),
            created_at: "2026-10-15T00:00:00Z".into(),
        }
    }

    #[test]
    fn the_text_of_a_reference_is_the_label_and_the_uri() {
        assert_eq!(
            reference_text(&reference("https://example.com/rfc", Some("RFC 42"))),
            "RFC 42 <https://example.com/rfc>"
        );
        assert_eq!(
            reference_text(&reference("https://example.com/rfc", None)),
            "<https://example.com/rfc>"
        );
        assert_eq!(
            reference_text(&reference("https://example.com/rfc", Some(""))),
            "<https://example.com/rfc>"
        );
    }

    #[test]
    fn the_stored_form_lowers_the_scheme_only() {
        assert_eq!(
            stored_form("  HTTPS://Example.com/A "),
            "https://Example.com/A"
        );
        assert_eq!(stored_form("no-scheme"), "no-scheme");
    }
}
