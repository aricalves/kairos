//! `kairos activity` (KAIROS-T-0592): the activity log of the organization,
//! newest first (`GET /api/activity`, KAIROS-A-0004). The filters combine.
//!
//! The server takes ids in its filters. The CLI takes the forms of
//! KAIROS-T-0589: a short code for the item, and an id, `me` or an email
//! for the actor. `<noun> history` gives the activity of one item with the
//! names of the people (KAIROS-T-0557).

use clap::Args;
use kairos_client::types::Pagination;
use kairos_client::types_meta::{ActivityEntry, ActivityQuery};
use kairos_client::{EntityKind, KairosClient};

use crate::commands::repos::family_of;
use crate::context::{Common, client, is_uuid};
use crate::error::CliError;
use crate::listing::{Listing, Paging, fetch};
use crate::table::Table;

/// The flags of `kairos activity`.
#[derive(Args, Debug)]
pub struct ActivityArgs {
    /// Only the activity of this item, by its short code (e.g. ACME-T-0001)
    #[arg(long, value_name = "CODE")]
    pub item: Option<String>,
    /// Only the activity of this person: an id (UUID), `me` or an email
    #[arg(long, value_name = "USER")]
    pub actor: Option<String>,
    /// Only this action, for example transition, create, delete, claim or
    /// update
    #[arg(long)]
    pub action: Option<String>,
    /// Only the activity at or after this time: a date (YYYY-MM-DD, UTC) or
    /// an RFC 3339 time
    #[arg(long, value_name = "TIME")]
    pub since: Option<String>,
    /// Only the activity of the members of this team (slug or UUID)
    #[arg(long)]
    pub team: Option<String>,
    /// Page size (server default 50, max 200)
    #[arg(long)]
    pub limit: Option<i64>,
    /// Rows to skip
    #[arg(long)]
    pub offset: Option<i64>,
    #[command(flatten)]
    pub common: Common,
}

impl ActivityArgs {
    pub async fn run(self) -> Result<(), CliError> {
        let client = client(&self.common)?;
        let entity_id = match &self.item {
            Some(code) => Some(item_id(&client, code).await?),
            None => None,
        };
        let actor_id = match &self.actor {
            Some(actor) => Some(user_id(&client, actor).await?),
            None => None,
        };
        let since = self.since.as_deref().map(since_value);
        let paging = Paging {
            limit: self.limit,
            offset: self.offset,
            all: false,
        };
        let listing = fetch(paging, |page: Pagination| {
            let query = ActivityQuery {
                entity_id: entity_id.clone(),
                actor_id: actor_id.clone(),
                action: self.action.clone(),
                since: since.clone(),
                team: self.team.clone(),
                limit: page.limit,
                offset: page.offset,
            };
            let client = &client;
            async move { client.activity(&query).await }
        })
        .await?;
        if self.common.json {
            return listing.print_json();
        }
        // The names of the people, from one page of the members. A caller
        // who cannot read the members sees the ids.
        let members = client
            .list_org_members(Pagination {
                limit: Some(crate::listing::ALL_PAGE),
                offset: None,
            })
            .await
            .map(|page| page.items)
            .unwrap_or_default();
        let names: Vec<(&str, &str)> = members
            .iter()
            .map(|m| (m.user_id.as_str(), m.display_name.as_str()))
            .collect();
        print!("{}", activity_text(&listing, &names));
        Ok(())
    }
}

/// The id of the item with the short code `code`: one read of the item.
pub async fn item_id(client: &KairosClient, code: &str) -> Result<String, CliError> {
    Ok(match family_of(code)? {
        EntityKind::Strategy => client.get_strategy(code).await?.id,
        EntityKind::Initiative => client.get_initiative(code).await?.id,
        EntityKind::Task => client.get_task(code).await?.id,
        EntityKind::Document => client.get_document(code).await?.id,
        EntityKind::Adr => client.get_adr(code).await?.id,
    })
}

/// The id of the person `user`: an id goes through as it is; `me` is the
/// caller; an email is found in the members of the organization.
pub async fn user_id(client: &KairosClient, user: &str) -> Result<String, CliError> {
    if is_uuid(user) {
        return Ok(user.to_string());
    }
    if user.eq_ignore_ascii_case("me") {
        return Ok(client.whoami().await?.user.id);
    }
    if !user.contains('@') {
        return Err(CliError::Failure(format!(
            "The value {user:?} of --actor is not an id, `me` or an email. Send one of them."
        )));
    }
    let paging = Paging {
        all: true,
        ..Paging::default()
    };
    let members = fetch(paging, |page| client.list_org_members(page)).await?;
    members
        .items
        .iter()
        .find(|m| m.email.eq_ignore_ascii_case(user))
        .map(|m| m.user_id.clone())
        .ok_or_else(|| {
            CliError::Failure(format!(
                "No member of the organization has the email {user:?}."
            ))
        })
}

/// The `since` of the query: a date is the start of that day in UTC; each
/// other value goes to the server as it is. Pure.
fn since_value(value: &str) -> String {
    if chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok() {
        format!("{value}T00:00:00Z")
    } else {
        value.to_string()
    }
}

/// The text of `kairos activity`: one row for each entry, then the footer.
/// `names` gives the name of a person by id. Pure.
fn activity_text(listing: &Listing<ActivityEntry>, names: &[(&str, &str)]) -> String {
    if listing.items.is_empty() {
        return format!("(none)\n{}", listing.footer());
    }
    let mut table = Table::new(&["WHEN", "ACTION", "ITEM", "DETAILS", "BY"]);
    for entry in &listing.items {
        let name = names
            .iter()
            .find(|(id, _)| *id == entry.actor_id)
            .map_or(entry.actor_id.as_str(), |(_, name)| *name);
        let by = if entry.agent_key_id.is_some() {
            format!("{name} (agent)")
        } else {
            name.to_string()
        };
        let item = match (&entry.entity_short_code, &entry.entity_archived_at) {
            (Some(code), Some(_)) => format!("{code} [archived]"),
            (Some(code), None) => code.clone(),
            (None, _) => "-".to_string(),
        };
        table.row(vec![
            second_text(&entry.occurred_at),
            entry.action.clone(),
            item,
            entry.details.clone(),
            by,
        ]);
    }
    format!("{}{}", table.render(), listing.footer())
}

/// An RFC 3339 time to the second, in UTC; another text as it is. Pure.
fn second_text(time: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(time).map_or_else(
        |_| time.to_string(),
        |t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_date_is_the_start_of_its_day() {
        assert_eq!(since_value("2026-10-18"), "2026-10-18T00:00:00Z");
        assert_eq!(
            since_value("2026-10-18T09:30:00+02:00"),
            "2026-10-18T09:30:00+02:00"
        );
    }

    #[test]
    fn the_activity_has_a_row_for_each_entry_with_names() {
        let entry = |actor: &str, agent: bool, code: Option<&str>| ActivityEntry {
            id: "a-1".into(),
            actor_id: actor.into(),
            action: "transition".into(),
            entity_id: None,
            entity_type: None,
            details: "column:Todo->Active".into(),
            occurred_at: "2026-10-18T10:00:00.123Z".into(),
            entity_short_code: code.map(str::to_string),
            entity_title: None,
            entity_archived_at: None,
            entity_repository: None,
            agent_key_id: agent.then(|| "k-1".to_string()),
        };
        let listing = Listing {
            items: vec![
                entry("u-1", true, Some("ACME-T-0001")),
                entry("u-2", false, None),
            ],
            total: 3,
            limit: 2,
            offset: 0,
            all: false,
        };
        let text = activity_text(&listing, &[("u-1", "Robin")]);
        assert!(
            text.contains(
                "2026-10-18T10:00:00Z  transition  ACME-T-0001  column:Todo->Active  Robin (agent)"
            ),
            "{text}"
        );
        assert!(
            text.contains("  -            column:Todo->Active  u-2"),
            "{text}"
        );
        assert!(text.contains("Next: --offset 2"), "{text}");
        let none = Listing::<ActivityEntry> {
            items: vec![],
            total: 0,
            limit: 50,
            offset: 0,
            all: false,
        };
        assert!(activity_text(&none, &[]).starts_with("(none)\n"));
    }
}
