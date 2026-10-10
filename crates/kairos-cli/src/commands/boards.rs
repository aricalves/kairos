//! `kairos boards` — list boards and render one board's items grouped by
//! column (KAIROS-A-0015: "board views per level with column layout";
//! `GET /api/boards` + `GET /api/boards/{id}/items`).

use crate::commands::entities::{EntityView, ListArgs};
use crate::context::{Common, client, print_json};
use crate::error::CliError;
use crate::table::Table;

use kairos_client::types_org::{BoardItemsQuery, BoardItemsResponse, CreateBoardRequest};

/// Operations on boards.
#[derive(clap::Subcommand, Debug)]
pub enum BoardsCommand {
    /// List boards (paginated)
    List(ListArgs),
    /// Create a board: an ADR board, a strategy or an initiative board, or the
    /// delivery board of a team that has none (KAIROS-T-0337)
    Create {
        /// Board name
        #[arg(long)]
        name: String,
        /// Board slug (^[a-z][a-z0-9_-]{1,62}$)
        #[arg(long)]
        slug: String,
        /// strategy|initiative|delivery|adr
        #[arg(long)]
        level: String,
        /// The owner team, by slug or id (UUID). A delivery board needs one. An
        /// ADR board with a team is the ADR board of that team
        #[arg(long)]
        team: Option<String>,
        /// The short-code prefix of the board, for example OPS (a capital
        /// letter, then 1 to 9 capital letters or digits). The ADR board of a
        /// team takes the prefix of the team. It does not change later.
        #[arg(long = "code-prefix", value_name = "PREFIX")]
        code_prefix: String,
        #[command(flatten)]
        common: Common,
    },
    /// Show a board's live items grouped by column (one page)
    Show {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board_id: String,
        /// Page size (server default 200, max 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Items to skip
        #[arg(long)]
        offset: Option<i64>,
        /// Show only the strategies and the initiatives of this team (slug
        /// or UUID), from tasks or set by hand. Tasks and ADRs are not
        /// changed
        #[arg(long, conflicts_with = "no_team")]
        team: Option<String>,
        /// Show only the strategies and the initiatives that have no team
        #[arg(long)]
        no_team: bool,
        /// Show more on each item: the date of the last change, the claim
        /// of a task and the metadata values, e.g. {updated 2026-10-08;
        /// priority: high}
        #[arg(long)]
        details: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Archive each task in the done columns of a delivery board, cancelled
    /// tasks too (requires --confirm). You can restore each task
    #[command(name = "archive-completed")]
    ArchiveCompleted {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// Archive the tasks
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

impl BoardsCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::Create {
                name,
                slug,
                level,
                team,
                code_prefix,
                common,
            } => {
                let client = client(&common)?;
                // The body takes the id of the team. A slug is read first.
                let team_id = match team {
                    Some(team) if looks_like_uuid(&team) => Some(team),
                    Some(team) => Some(client.get_team_by_slug(&team).await?.id),
                    None => None,
                };
                let detail = client
                    .create_board(&CreateBoardRequest {
                        name,
                        slug,
                        board_level: level,
                        team_id,
                        code_prefix,
                    })
                    .await?;
                if common.json {
                    return print_json(&detail);
                }
                let columns: Vec<&str> = detail.columns.iter().map(|c| c.name.as_str()).collect();
                println!(
                    "Created board {} ({}, {}, prefix {}). Columns: {}.",
                    detail.board.slug,
                    detail.board.name,
                    detail.board.board_level,
                    detail.board.code_prefix,
                    columns.join(", ")
                );
                Ok(())
            }
            Self::List(args) => {
                let client = client(&args.common)?;
                let envelope = client.list_boards(args.page()).await?;
                if args.common.json {
                    return print_json(&envelope);
                }
                if envelope.items.is_empty() {
                    println!("(none)");
                } else {
                    let mut table = Table::new(&["ID", "NAME", "SLUG", "LEVEL", "TEAM"]);
                    for board in &envelope.items {
                        table.row(vec![
                            board.id.clone(),
                            board.name.clone(),
                            board.slug.clone(),
                            board.board_level.clone(),
                            board.team_id.clone().unwrap_or_else(|| "-".to_string()),
                        ]);
                    }
                    print!("{}", table.render());
                }
                println!(
                    "total: {} (limit {}, offset {})",
                    envelope.total, envelope.limit, envelope.offset
                );
                Ok(())
            }
            Self::Show {
                board_id,
                limit,
                offset,
                team,
                no_team,
                details,
                common,
            } => {
                let client = client(&common)?;
                // COLLIERY-T-0261: one page of the board.
                let query = BoardItemsQuery {
                    limit,
                    offset,
                    team,
                    no_team,
                    with_metadata: details,
                    ..BoardItemsQuery::default()
                };
                let items = client.board_items(&board_id, &query).await?;
                if common.json {
                    return print_json(&items);
                }
                print_board_items(&items, details);
                println!();
                println!(
                    "total: {} (limit {}, offset {})",
                    items.total, items.limit, items.offset
                );
                // The page is a part of the board: say so, and say how to
                // read the next part.
                if let Some(note) = items.incomplete_note("use --offset") {
                    println!("{note}");
                }
                Ok(())
            }
            Self::ArchiveCompleted {
                board,
                confirm,
                common,
            } => {
                if !confirm {
                    return Err(CliError::Failure(format!(
                        "The archive of the completed tasks of {board} needs --confirm. Run \
                         the command again with --confirm."
                    )));
                }
                let client = client(&common)?;
                let response = client.archive_completed(&board).await?;
                if common.json {
                    return print_json(&response);
                }
                println!("{}", archived_completed_text(&board, &response));
                Ok(())
            }
        }
    }
}

/// The human board rendering: one section per column (in position order),
/// every live item listed with its kind, short code, and title.
fn print_board_items(items: &BoardItemsResponse, details: bool) {
    // KAIROS-T-0368: with --details, the date of the last change, the
    // claim of a task and the metadata values.
    let more = |code: &str, updated_at: &str, claim: Option<&kairos_client::types::TaskClaim>| {
        if !details {
            return String::new();
        }
        let mut parts = vec![format!(
            "updated {}",
            updated_at.get(..10).unwrap_or(updated_at)
        )];
        if let Some(claim) = claim {
            parts.push(format!(
                "claimed by {}{}",
                claim.display_name,
                if claim.agent { " (agent)" } else { "" }
            ));
        }
        if let Some(values) = items.item_metadata.as_ref().and_then(|m| m.get(code)) {
            parts.extend(
                values
                    .iter()
                    .map(|(slug, value)| format!("{slug}: {value}")),
            );
        }
        format!("  {{{}}}", parts.join("; "))
    };
    println!(
        "Board: {} (slug {}, level {}, id {})",
        items.board.name, items.board.slug, items.board.board_level, items.board.id
    );
    for column in &items.columns {
        let count = column.strategies.len()
            + column.initiatives.len()
            + column.tasks.len()
            + column.adrs.len();
        println!();
        println!(
            "== {} ({count} items, id {})",
            column.column.name, column.column.id
        );
        if count == 0 {
            println!("   (empty)");
            continue;
        }
        for item in &column.strategies {
            println!(
                "   {}  [strategy] {}{}{}",
                item.short_code(),
                item.title(),
                teams_suffix(items, item.short_code()),
                more(item.short_code(), &item.updated_at, None)
            );
        }
        for item in &column.initiatives {
            println!(
                "   {}  [initiative] {}{}{}",
                item.short_code(),
                item.title(),
                teams_suffix(items, item.short_code()),
                more(item.short_code(), &item.updated_at, None)
            );
        }
        for item in &column.tasks {
            // KAIROS-T-0077: the Support lane rides on the type tag;
            // Planned stays unmarked as the default lane.
            let lane = if item.work_class == "support" {
                " (support lane)"
            } else {
                ""
            };
            println!(
                "   {}  [{}]{lane} {}{}",
                item.short_code(),
                item.task_type,
                item.title(),
                more(item.short_code(), &item.updated_at, item.claim.as_ref())
            );
        }
        for item in &column.adrs {
            println!(
                "   {}  [adr] {}{}",
                item.short_code(),
                item.title(),
                more(item.short_code(), &item.updated_at, None)
            );
        }
    }
}

/// The teams of a strategy or an initiative, after its title
/// (KAIROS-T-0321): `  [teams: a, b]`, or nothing when it has no team.
fn teams_suffix(items: &BoardItemsResponse, short_code: &str) -> String {
    match items.item_teams.get(short_code) {
        Some(teams) if !teams.is_empty() => format!(
            "  [teams: {}]",
            teams
                .iter()
                .map(|team| team.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => String::new(),
    }
}

/// The text of `boards archive-completed` (KAIROS-T-0363).
fn archived_completed_text(
    board: &str,
    response: &kairos_client::types::ArchiveCompletedResponse,
) -> String {
    if response.count == 0 {
        return format!("The done columns of {board} have no tasks. Kairos archived nothing.");
    }
    format!(
        "Kairos archived {} completed task{} of {board}: {}.\nTo get one back, run `kairos \
         tasks restore <code>`.",
        response.count,
        if response.count == 1 { "" } else { "s" },
        response.short_codes.join(", ")
    )
}

/// Whether `value` has the form of a UUID (8-4-4-4-12 hex digits). A team
/// slug cannot have that form (COLLIERY-T-0260).
fn looks_like_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(part, len)| part.len() == len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod archive_completed_tests {
    use super::*;

    /// KAIROS-T-0363: the text gives the count and the codes, and says
    /// how to restore.
    #[test]
    fn the_text_gives_the_count() {
        let response = |codes: &[&str]| kairos_client::types::ArchiveCompletedResponse {
            board_id: "b".into(),
            count: codes.len() as i64,
            short_codes: codes.iter().map(|c| c.to_string()).collect(),
        };
        assert_eq!(
            archived_completed_text("acme", &response(&[])),
            "The done columns of acme have no tasks. Kairos archived nothing."
        );
        let text = archived_completed_text("acme", &response(&["ACME-T-0001", "ACME-T-0002"]));
        assert!(
            text.starts_with(
                "Kairos archived 2 completed tasks of acme: ACME-T-0001, ACME-T-0002."
            ),
            "{text}"
        );
        assert!(text.contains("tasks restore"), "{text}");
    }
}
