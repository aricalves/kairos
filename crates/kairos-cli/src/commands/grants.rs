//! `kairos grants` — capability grants (KAIROS-T-0337). `manage_structure`
//! is the structure of the organization: an organization admin grants it to
//! a person or to a service account, on the organization or on one board
//! (`/api/structure-grants`). A board capability is a grant on one board
//! (`POST /api/boards/{id}/members`).

use kairos_client::types_org::{
    AddBoardMemberRequest, CreateStructureGrantRequest, StructureGrant,
};

use crate::context::{Common, client, print_json};
use crate::error::CliError;
use crate::table::Table;

/// The name of the structure capability.
const MANAGE_STRUCTURE: &str = "manage_structure";

/// Capability grants: `manage_structure`, and the board capabilities.
#[derive(clap::Subcommand, Debug)]
pub enum GrantsCommand {
    /// List each grant of manage_structure (on the organization and on boards)
    List {
        #[command(flatten)]
        common: Common,
    },
    /// Grant a capability. manage_structure with no --board: on the
    /// organization (teams, boards, members, grants, repositories). With
    /// --board: that board only. Only an organization admin grants
    /// manage_structure. A board capability needs --board
    Add {
        /// The member. For manage_structure: a user id, an email, a user name
        /// or the name of a service account. For a board capability: the user
        /// id (UUID)
        #[arg(long)]
        user: String,
        /// manage_structure, or a board capability (manage_tasks,
        /// configure_boards, administer_members, ...)
        #[arg(long, default_value = MANAGE_STRUCTURE)]
        capability: String,
        /// The board, by slug or id (UUID)
        #[arg(long)]
        board: Option<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Revoke a grant of manage_structure (requires --confirm)
    Remove {
        /// The id of the grant (UUID; see `kairos grants list`)
        grant_id: String,
        /// Revoke it
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

/// The scope of a grant, as a person reads it.
fn scope_text(grant: &StructureGrant) -> String {
    match &grant.board_slug {
        Some(board) => format!("board {board}"),
        None => "organization".to_string(),
    }
}

impl GrantsCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List { common } => {
                let client = client(&common)?;
                let grants = client.list_structure_grants().await?;
                if common.json {
                    return print_json(&grants);
                }
                if grants.is_empty() {
                    println!("(none)");
                    return Ok(());
                }
                let mut table = Table::new(&["ID", "MEMBER", "KIND", "SCOPE", "GRANTED"]);
                for grant in &grants {
                    table.row(vec![
                        grant.id.clone(),
                        format!("{} <{}>", grant.display_name, grant.email),
                        grant.kind.clone(),
                        scope_text(grant),
                        grant.granted_at.clone(),
                    ]);
                }
                print!("{}", table.render());
                Ok(())
            }
            Self::Add {
                user,
                capability,
                board,
                common,
            } => {
                let client = client(&common)?;
                if capability == MANAGE_STRUCTURE {
                    let grant = client
                        .create_structure_grant(&CreateStructureGrantRequest { user, board })
                        .await?;
                    if common.json {
                        return print_json(&grant);
                    }
                    println!(
                        "Granted manage_structure on the {} to {} <{}> (grant {}).",
                        scope_text(&grant),
                        grant.display_name,
                        grant.email,
                        grant.id
                    );
                    return Ok(());
                }
                let Some(board) = board else {
                    return Err(CliError::Failure(format!(
                        "The capability {capability:?} is a board capability. Send --board."
                    )));
                };
                let member = client
                    .add_board_member(
                        &board,
                        &AddBoardMemberRequest {
                            user_id: user,
                            capabilities: vec![capability.clone()],
                        },
                    )
                    .await?;
                if common.json {
                    return print_json(&member);
                }
                println!(
                    "Granted {capability} on the board {board} to {} <{}>. The capabilities of \
                     the member on the board: {}.",
                    member.display_name,
                    member.email,
                    member.capabilities.join(", ")
                );
                Ok(())
            }
            Self::Remove {
                grant_id,
                confirm,
                common,
            } => {
                if !confirm {
                    return Err(CliError::Failure(format!(
                        "The revoke of the grant {grant_id} needs --confirm. Run the command \
                         again with --confirm."
                    )));
                }
                let client = client(&common)?;
                let grant = client.delete_structure_grant(&grant_id).await?;
                if common.json {
                    return print_json(&grant);
                }
                println!(
                    "Revoked manage_structure on the {} from {} <{}>.",
                    scope_text(&grant),
                    grant.display_name,
                    grant.email
                );
                Ok(())
            }
        }
    }
}
