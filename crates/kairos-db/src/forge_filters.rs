//! The forge filters of a repository (KAIROS-T-0479, COLLIERY-I-0623):
//! storage of the include and exclude patterns per field. The rules are in
//! [`kairos_core::forge_filter`]; a field with no row has the default.
//!
//! Every function operates in the CURRENT `search_path` tenant schema.

use chrono::Utc;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use kairos_core::forge_filter::{FilterError, FilterField, FilterRule, ForgeFilter};
use uuid::Uuid;

use crate::models::enums::ActivityAction;
use crate::models::graph::NewActivityLogEntry;
use crate::schema::repository_forge_filters as rff;

/// Errors of the filter store.
#[derive(Debug, thiserror::Error)]
pub enum FilterStoreError {
    #[error(transparent)]
    Pattern(#[from] FilterError),
    #[error("The database gave an error: {0}.")]
    Database(#[from] diesel::result::Error),
}

/// The rule of each field (the default where no row is stored), in the
/// order of [`FilterField::ALL`].
pub fn rules(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> QueryResult<Vec<(FilterField, FilterRule)>> {
    let rows: Vec<(String, String, String)> = rff::table
        .filter(rff::repository_id.eq(repository_id))
        .select((rff::field, rff::include_pattern, rff::exclude_pattern))
        .load(conn)?;
    Ok(FilterField::ALL
        .into_iter()
        .map(|field| {
            let rule = rows
                .iter()
                .find(|(name, _, _)| name == field.as_str())
                .map(|(_, include, exclude)| FilterRule {
                    include: include.clone(),
                    exclude: exclude.clone(),
                })
                .unwrap_or_default();
            (field, rule)
        })
        .collect())
}

/// The compiled filter of a repository. A stored pattern that no longer
/// compiles (it was checked on save) is ignored for that field, with a
/// warning: a bad row must not stop the intake of every event.
pub fn filter(conn: &mut PgConnection, repository_id: Uuid) -> QueryResult<ForgeFilter> {
    let rules = rules(conn, repository_id)?;
    let mut good = Vec::new();
    for (field, rule) in rules {
        match kairos_core::forge_filter::validate(field, &rule) {
            Ok(()) => good.push((field, rule)),
            Err(e) => tracing::warn!(%repository_id, error = %e, "ignoring a stored forge filter"),
        }
    }
    Ok(ForgeFilter::new(good).unwrap_or_default())
}

/// Set the rules of some fields. Each pattern is checked first; nothing is
/// written when one is refused. A default rule removes the row. Returns the
/// names of the fields whose rule changed (an activity row names them).
pub fn set(
    conn: &mut PgConnection,
    repository_id: Uuid,
    repository_slug: &str,
    changes: &[(FilterField, FilterRule)],
    actor: Uuid,
) -> Result<Vec<&'static str>, FilterStoreError> {
    for (field, rule) in changes {
        kairos_core::forge_filter::validate(*field, rule)?;
    }
    let current = rules(conn, repository_id)?;
    let mut changed = Vec::new();
    conn.transaction::<_, FilterStoreError, _>(|conn| {
        for (field, rule) in changes {
            let old = current
                .iter()
                .find(|(f, _)| f == field)
                .map(|(_, r)| r.clone())
                .unwrap_or_default();
            if same(&old, rule) {
                continue;
            }
            diesel::delete(
                rff::table
                    .filter(rff::repository_id.eq(repository_id))
                    .filter(rff::field.eq(field.as_str())),
            )
            .execute(conn)?;
            if !rule.is_default() {
                diesel::insert_into(rff::table)
                    .values((
                        rff::repository_id.eq(repository_id),
                        rff::field.eq(field.as_str()),
                        rff::include_pattern.eq(&rule.include),
                        rff::exclude_pattern.eq(&rule.exclude),
                        rff::updated_by.eq(actor),
                        rff::updated_at.eq(Utc::now()),
                    ))
                    .execute(conn)?;
            }
            changed.push(field.as_str());
        }
        if !changed.is_empty() {
            diesel::insert_into(crate::schema::activity_log::table)
                .values(NewActivityLogEntry {
                    actor_id: actor,
                    action: ActivityAction::Repository,
                    entity_id: Some(repository_id),
                    entity_type: Some("repository".to_string()),
                    details: format!(
                        "repository_forge_filters:{repository_slug}:{}",
                        changed.join(",")
                    ),
                })
                .execute(conn)?;
        }
        Ok(())
    })?;
    Ok(changed)
}

/// Two rules that admit the same: an empty include is `.*`.
fn same(a: &FilterRule, b: &FilterRule) -> bool {
    let include = |r: &FilterRule| {
        if r.include.is_empty() {
            kairos_core::forge_filter::DEFAULT_INCLUDE.to_string()
        } else {
            r.include.clone()
        }
    };
    include(a) == include(b) && a.exclude == b.exclude
}

/// The stored rows of many repositories in ONE query: repository id →
/// (field, rule). A field with no row is absent; the caller fills the
/// default.
pub fn stored_for(
    conn: &mut PgConnection,
    repository_ids: &[Uuid],
) -> QueryResult<std::collections::HashMap<Uuid, Vec<(FilterField, FilterRule)>>> {
    let rows: Vec<(Uuid, String, String, String)> = rff::table
        .filter(rff::repository_id.eq_any(repository_ids))
        .select((
            rff::repository_id,
            rff::field,
            rff::include_pattern,
            rff::exclude_pattern,
        ))
        .load(conn)?;
    let mut out: std::collections::HashMap<Uuid, Vec<(FilterField, FilterRule)>> =
        std::collections::HashMap::new();
    for (id, field, include, exclude) in rows {
        if let Some(field) = FilterField::parse(&field) {
            out.entry(id)
                .or_default()
                .push((field, FilterRule { include, exclude }));
        }
    }
    Ok(out)
}
