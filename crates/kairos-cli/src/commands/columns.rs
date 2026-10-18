//! `kairos boards columns` and `kairos boards transitions` (KAIROS-T-0443):
//! the columns of a board and the moves between them. Each command calls the
//! REST route of the board; a column is a name or an id (UUID).

use kairos_client::KairosClient;
use kairos_client::types_org::{
    BoardDetail, CreateColumnRequest, CreateTransitionRequest, UpdateColumnRequest,
};

use crate::commands::entities::require_confirm;
use crate::context::{Common, client, find_column, print_json};
use crate::error::CliError;
use crate::listing::print_whole;
use crate::table::Table;

/// The columns of a board.
#[derive(clap::Subcommand, Debug)]
pub enum ColumnsCommand {
    /// List the columns of a board, in their order
    List {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        #[command(flatten)]
        common: Common,
    },
    /// Add a column (needs configure_boards). It has no transition: add
    /// them with `kairos boards transitions add`
    Add {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// The name of the new column
        #[arg(value_name = "NAME")]
        name: String,
        /// The position, from 0 (the first). Default: the last
        #[arg(long)]
        position: Option<i32>,
        #[command(flatten)]
        common: Common,
    },
    /// Change a column (needs configure_boards): its name, position, done
    /// flag or claims flags. `kairos boards wip-limit` sets the WIP limit
    Update {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// Column name or id (UUID)
        #[arg(value_name = "COLUMN")]
        column: String,
        /// The new name
        #[arg(long)]
        name: Option<String>,
        /// The new position, from 0 (the first)
        #[arg(long)]
        position: Option<i32>,
        /// true: a done column
        #[arg(long)]
        done: Option<bool>,
        /// true: a task in the column has a claim
        #[arg(long)]
        claims: Option<bool>,
        /// true: a task keeps its claim in the column and gets none there (Blocked)
        #[arg(long = "keeps-claims")]
        keeps_claims: Option<bool>,
        #[command(flatten)]
        common: Common,
    },
    /// Remove a column (needs configure_boards and --confirm). It must
    /// hold no live item
    Remove {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// Column name or id (UUID)
        #[arg(value_name = "COLUMN")]
        column: String,
        /// Required: without it, the command deletes nothing
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

/// The moves between the columns of a board.
#[derive(clap::Subcommand, Debug)]
pub enum TransitionsCommand {
    /// List the allowed moves of a board
    List {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        #[command(flatten)]
        common: Common,
    },
    /// Allow the move FROM -> TO (needs configure_boards)
    Add {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// The column where the move starts: name or id (UUID)
        #[arg(value_name = "FROM")]
        from: String,
        /// The column where the move ends: name or id (UUID)
        #[arg(value_name = "TO")]
        to: String,
        #[command(flatten)]
        common: Common,
    },
    /// Stop the move FROM -> TO (needs configure_boards and --confirm)
    Remove {
        /// Board slug or id (UUID)
        #[arg(value_name = "BOARD")]
        board: String,
        /// The column where the move starts: name or id (UUID)
        #[arg(value_name = "FROM")]
        from: String,
        /// The column where the move ends: name or id (UUID)
        #[arg(value_name = "TO")]
        to: String,
        /// Required: without it, the command deletes nothing
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

fn column_name(detail: &BoardDetail, id: &str) -> String {
    detail
        .columns
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| id.to_string())
}

fn columns_table(detail: &BoardDetail) -> Table {
    let mut table = Table::new(&[
        "POSITION",
        "COLUMN",
        "DONE",
        "CLAIMS",
        "KEEPS CLAIMS",
        "WIP LIMIT",
        "ID",
    ]);
    for c in &detail.columns {
        table.row(vec![
            c.position.to_string(),
            c.name.clone(),
            if c.is_done { "yes" } else { "no" }.into(),
            if c.claims { "yes" } else { "no" }.into(),
            if c.keeps_claims { "yes" } else { "no" }.into(),
            c.wip_limit
                .map(|limit| limit.to_string())
                .unwrap_or_else(|| "-".into()),
            c.id.clone(),
        ]);
    }
    table
}

async fn board(client: &KairosClient, board: &str) -> Result<BoardDetail, CliError> {
    Ok(client.get_board(board).await?)
}

impl ColumnsCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List { board: b, common } => {
                let client = client(&common)?;
                let detail = board(&client, &b).await?;
                if common.json {
                    return print_whole(&detail.columns);
                }
                print!("{}", columns_table(&detail).render());
                Ok(())
            }
            Self::Add {
                board: b,
                name,
                position,
                common,
            } => {
                let client = client(&common)?;
                let detail = board(&client, &b).await?;
                let position = position.unwrap_or(detail.columns.len() as i32);
                let column = client
                    .add_column(&detail.board.id, &CreateColumnRequest { name, position })
                    .await?;
                if common.json {
                    return print_json(&column);
                }
                println!(
                    "Added the column {} ({}) to the board {} at position {}.",
                    column.name, column.id, detail.board.slug, column.position
                );
                Ok(())
            }
            Self::Update {
                board: b,
                column,
                name,
                position,
                done,
                claims,
                keeps_claims,
                common,
            } => {
                if name.is_none()
                    && position.is_none()
                    && done.is_none()
                    && claims.is_none()
                    && keeps_claims.is_none()
                {
                    return Err(CliError::Failure(
                        "The command has no change. Use --name, --position, --done, --claims or \
                         --keeps-claims."
                            .to_string(),
                    ));
                }
                let client = client(&common)?;
                let detail = board(&client, &b).await?;
                let target = find_column(&detail, &column)?;
                let updated = client
                    .update_column(
                        &detail.board.id,
                        &target.id,
                        &UpdateColumnRequest {
                            name,
                            position,
                            is_done: done,
                            claims,
                            keeps_claims,
                            wip_limit: None,
                        },
                    )
                    .await?;
                if common.json {
                    return print_json(&updated);
                }
                println!(
                    "The column {} of the board {}: position {}, done {}, claims {}, keeps claims {}.",
                    updated.name,
                    detail.board.slug,
                    updated.position,
                    updated.is_done,
                    updated.claims,
                    updated.keeps_claims
                );
                Ok(())
            }
            Self::Remove {
                board: b,
                column,
                confirm,
                common,
            } => {
                require_confirm(confirm, &format!("the column {column} of the board {b}"))?;
                let client = client(&common)?;
                let detail = board(&client, &b).await?;
                let target = find_column(&detail, &column)?;
                let removed = client.remove_column(&detail.board.id, &target.id).await?;
                if common.json {
                    return print_json(&removed);
                }
                println!(
                    "Removed the column {} of the board {}.",
                    target.name, detail.board.slug
                );
                Ok(())
            }
        }
    }
}

impl TransitionsCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List { board: b, common } => {
                let client = client(&common)?;
                let detail = board(&client, &b).await?;
                if common.json {
                    return print_whole(&detail.transitions);
                }
                let mut table = Table::new(&["FROM", "TO", "ID"]);
                for t in &detail.transitions {
                    table.row(vec![
                        column_name(&detail, &t.from_column_id),
                        column_name(&detail, &t.to_column_id),
                        t.id.clone(),
                    ]);
                }
                print!("{}", table.render());
                Ok(())
            }
            Self::Add {
                board: b,
                from,
                to,
                common,
            } => {
                let client = client(&common)?;
                let detail = board(&client, &b).await?;
                let from_column = find_column(&detail, &from)?;
                let to_column = find_column(&detail, &to)?;
                let created = client
                    .add_transition(
                        &detail.board.id,
                        &CreateTransitionRequest {
                            from_column_id: from_column.id.clone(),
                            to_column_id: to_column.id.clone(),
                        },
                    )
                    .await?;
                if common.json {
                    return print_json(&created);
                }
                println!(
                    "The board {} allows the move {} -> {}.",
                    detail.board.slug, from_column.name, to_column.name
                );
                Ok(())
            }
            Self::Remove {
                board: b,
                from,
                to,
                confirm,
                common,
            } => {
                require_confirm(
                    confirm,
                    &format!("the move {from} -> {to} of the board {b}"),
                )?;
                let client = client(&common)?;
                let detail = board(&client, &b).await?;
                let from_column = find_column(&detail, &from)?;
                let to_column = find_column(&detail, &to)?;
                let edge = detail
                    .transitions
                    .iter()
                    .find(|t| t.from_column_id == from_column.id && t.to_column_id == to_column.id)
                    .ok_or_else(|| {
                        CliError::Failure(format!(
                            "The board {} has no move {} -> {}.",
                            detail.board.slug, from_column.name, to_column.name
                        ))
                    })?;
                let removed = client.remove_transition(&detail.board.id, &edge.id).await?;
                if common.json {
                    return print_json(&removed);
                }
                println!(
                    "The board {} does not allow the move {} -> {} now.",
                    detail.board.slug, from_column.name, to_column.name
                );
                Ok(())
            }
        }
    }
}
