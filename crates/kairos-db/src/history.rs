//! The rows of `item_history`: write a version, read a version
//! (KAIROS-T-0651, amends KAIROS-A-0004).
//!
//! A version is a full row or a diff row ([`kairos_core::history_diff`]).
//! A diff row applies to the newest full row of its item before it, so a
//! read needs at most two rows. [`insert_version`] chooses the form of a
//! new version. [`version_row`] and [`with_full_content`] give rows with
//! the full text in `content`. Each reader of a version text must use
//! them: the `content` of a diff row is empty.
//!
//! The rows from before KAIROS-T-0651 are full rows. They stay as they are.

use std::collections::{HashMap, HashSet};

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use uuid::Uuid;

use kairos_core::history_diff::{self, StoredForm};

use crate::models::graph::{ItemHistory, NewItemHistory, NewItemHistoryDiff};
use crate::schema::item_history;

/// Errors of a read of a version.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// The diff row has no base full row, or its diff does not fit the
    /// base. The text of this version cannot be read.
    #[error("The server cannot read the version {version} of the item {item_id}. {reason}")]
    Corrupt {
        item_id: Uuid,
        version: i32,
        reason: String,
    },
    /// Any other database error.
    #[error("The database gave an error: {0}.")]
    Database(#[from] DieselError),
}

/// Append the version `version` of the item `item_id`, with the full title
/// and the text `content`. A full row or a diff row, as
/// [`history_diff::plan_write`] chooses. The title is always full.
pub fn insert_version(
    conn: &mut PgConnection,
    item_id: Uuid,
    version: i32,
    title: &str,
    content: &str,
    actor: Uuid,
) -> Result<(), DieselError> {
    let base: Option<(i32, String)> = item_history::table
        .filter(item_history::item_id.eq(item_id))
        .filter(item_history::base_version.is_null())
        .filter(item_history::version.lt(version))
        .order(item_history::version.desc())
        .select((item_history::version, item_history::content))
        .first(conn)
        .optional()?;
    let form = history_diff::plan_write(
        version,
        base.as_ref().map(|(v, text)| (*v, text.as_str())),
        content,
    );
    match form {
        StoredForm::Full => diesel::insert_into(item_history::table)
            .values(NewItemHistory {
                item_id,
                version,
                title: title.to_string(),
                content: content.to_string(),
                edited_by: actor,
            })
            .execute(conn)?,
        StoredForm::Diff { base_version, diff } => diesel::insert_into(item_history::table)
            .values(NewItemHistoryDiff {
                item_id,
                version,
                title: title.to_string(),
                content: String::new(),
                edited_by: actor,
                base_version,
                content_diff: diff,
            })
            .execute(conn)?,
    };
    Ok(())
}

/// The version `version` of the item `item_id`, with the full text in
/// `content`, or `None` when the history has no such version.
pub fn version_row(
    conn: &mut PgConnection,
    item_id: Uuid,
    version: i32,
) -> Result<Option<ItemHistory>, HistoryError> {
    let row: Option<ItemHistory> = item_history::table
        .filter(item_history::item_id.eq(item_id))
        .filter(item_history::version.eq(version))
        .select(ItemHistory::as_select())
        .first(conn)
        .optional()?;
    match row {
        None => Ok(None),
        Some(row) => Ok(with_full_content(conn, vec![row])?.pop()),
    }
}

/// `rows` with the full text in `content`: the text of each diff row comes
/// from its base full row. A full row does not change. The order stays.
pub fn with_full_content(
    conn: &mut PgConnection,
    mut rows: Vec<ItemHistory>,
) -> Result<Vec<ItemHistory>, HistoryError> {
    let wanted: HashSet<(Uuid, i32)> = rows
        .iter()
        .filter_map(|row| row.base_version.map(|base| (row.item_id, base)))
        .collect();
    if wanted.is_empty() {
        return Ok(rows);
    }
    let bases = base_texts(conn, &wanted)?;
    for row in &mut rows {
        if let (Some(base_version), Some(diff)) = (row.base_version, row.content_diff.as_deref()) {
            row.content = full_text(&bases, row.item_id, row.version, base_version, diff)?;
        }
    }
    Ok(rows)
}

/// The texts of the full rows `wanted`, as `(item_id, version)`. A key
/// that is not a full row is not in the map.
pub fn base_texts(
    conn: &mut PgConnection,
    wanted: &HashSet<(Uuid, i32)>,
) -> Result<HashMap<(Uuid, i32), String>, DieselError> {
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let items: Vec<Uuid> = wanted.iter().map(|(item, _)| *item).collect();
    let versions: Vec<i32> = wanted.iter().map(|(_, version)| *version).collect();
    Ok(item_history::table
        .filter(item_history::item_id.eq_any(&items))
        .filter(item_history::version.eq_any(&versions))
        .filter(item_history::base_version.is_null())
        .select((
            item_history::item_id,
            item_history::version,
            item_history::content,
        ))
        .load::<(Uuid, i32, String)>(conn)?
        .into_iter()
        .filter(|(item, version, _)| wanted.contains(&(*item, *version)))
        .map(|(item, version, text)| ((item, version), text))
        .collect())
}

/// The full text of the diff row `version` of `item_id`: `diff` applied
/// to the base text from `bases` ([`base_texts`]).
pub fn full_text(
    bases: &HashMap<(Uuid, i32), String>,
    item_id: Uuid,
    version: i32,
    base_version: i32,
    diff: &str,
) -> Result<String, HistoryError> {
    let corrupt = |reason: String| HistoryError::Corrupt {
        item_id,
        version,
        reason,
    };
    let base = bases
        .get(&(item_id, base_version))
        .ok_or_else(|| corrupt(format!("Its base version {base_version} is missing.")))?;
    history_diff::apply(base, diff).map_err(|e| corrupt(e.to_string()))
}
