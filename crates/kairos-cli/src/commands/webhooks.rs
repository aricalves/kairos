//! `kairos repos webhook` (KAIROS-T-0592): the webhook connection of a
//! repository (`/api/forge-connections`, KAIROS-T-0097). An organization
//! admin connects a repository, lists the connections, rotates the secret
//! and removes a connection.
//!
//! The server makes the secret from the id of the connection and does not
//! keep it. So `add` and `rotate` print the secret one time, and no other
//! command prints it. A rotate makes a new connection: the URL and the
//! secret both change, and the old ones stop.

use kairos_client::KairosClient;
use kairos_client::types_forge::{
    CreateForgeConnectionRequest, CreatedForgeConnection, ForgeConnection,
};

use crate::commands::entities::require_confirm;
use crate::context::{Common, client, print_json};
use crate::error::CliError;
use crate::listing::print_whole;
use crate::table::Table;

/// The webhook connection of a repository.
#[derive(clap::Subcommand, Debug)]
pub enum WebhookCommand {
    /// Connect the webhook of a repository on GitHub or GitLab (org admin).
    /// The command prints the payload URL and the secret one time: paste
    /// them into the webhook settings of the repository on the forge
    Add {
        /// Repository slug (or UUID)
        repository: String,
        #[command(flatten)]
        common: Common,
    },
    /// List the webhook connections, or the connection of one repository.
    /// The list shows no secret
    List {
        /// Only the connection of this repository (slug or UUID)
        repository: Option<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Make a new payload URL and secret for the webhook of a repository
    /// (org admin). The old URL and secret stop at once. The command prints
    /// the new ones one time
    Rotate {
        /// Repository slug (or UUID)
        repository: String,
        #[command(flatten)]
        common: Common,
    },
    /// Remove the webhook connection of a repository (org admin; requires
    /// --confirm). Kairos then refuses the deliveries of the forge
    Remove {
        /// Repository slug (or UUID)
        repository: String,
        /// Remove the connection
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

impl WebhookCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::Add { repository, common } => {
                let client = client(&common)?;
                let created = client
                    .create_forge_connection(&CreateForgeConnectionRequest {
                        repository: repository.clone(),
                    })
                    .await?;
                if common.json {
                    return print_json(&created);
                }
                print!("{}", secret_text("connected", &repository, &created));
                Ok(())
            }
            Self::List { repository, common } => {
                let client = client(&common)?;
                let mut connections = client.list_forge_connections().await?;
                if let Some(repository) = &repository {
                    connections.retain(|c| is_of(c, repository));
                }
                if common.json {
                    return print_whole(&connections);
                }
                print!("{}", list_text(&connections));
                Ok(())
            }
            Self::Rotate { repository, common } => {
                let client = client(&common)?;
                let id = connection_of(&client, &repository).await?;
                let created = client.rotate_forge_connection(&id).await?;
                if common.json {
                    return print_json(&created);
                }
                print!("{}", secret_text("rotated", &repository, &created));
                Ok(())
            }
            Self::Remove {
                repository,
                confirm,
                common,
            } => {
                require_confirm(
                    confirm,
                    &format!("the webhook connection of the repository {repository}"),
                )?;
                let client = client(&common)?;
                let id = connection_of(&client, &repository).await?;
                let deleted = client.delete_forge_connection(&id).await?;
                if common.json {
                    return print_json(&deleted);
                }
                println!(
                    "Kairos removed the webhook connection of the repository {repository}. \
                     Remove the webhook on the forge too."
                );
                Ok(())
            }
        }
    }
}

/// True when the connection is the connection of `repository` (a slug or
/// an id). Pure.
fn is_of(connection: &ForgeConnection, repository: &str) -> bool {
    connection.repository.slug == repository || connection.repository.id == repository
}

/// The id of the connection of `repository`. A repository with no
/// connection is an error that says how to connect it.
async fn connection_of(client: &KairosClient, repository: &str) -> Result<String, CliError> {
    let connections = client.list_forge_connections().await?;
    connections
        .iter()
        .find(|c| is_of(c, repository))
        .map(|c| c.id.clone())
        .ok_or_else(|| {
            CliError::Failure(format!(
                "The repository {repository} has no webhook connection. To connect it, run: \
                 kairos repos webhook add {repository}"
            ))
        })
}

/// The text of `add` and `rotate`: the URL and the secret, one time. Pure.
fn secret_text(done: &str, repository: &str, created: &CreatedForgeConnection) -> String {
    format!(
        "Kairos {done} the webhook of the repository {repository} ({forge}).\n\
         Paste these values into the webhook settings of the repository on {forge}:\n  \
         Payload URL: {url}\n  \
         Secret:      {secret}\n\
         Kairos shows the secret only now. To get a new secret, run: kairos repos webhook rotate \
         {repository}\n",
        forge = created.connection.forge,
        url = created.webhook_url,
        secret = created.webhook_secret,
    )
}

/// The text of `list`. Pure.
fn list_text(connections: &[ForgeConnection]) -> String {
    if connections.is_empty() {
        return "(none)\ntotal: 0\n".to_string();
    }
    let mut table = Table::new(&["ID", "REPOSITORY", "FORGE", "NAME", "CREATED"]);
    for connection in connections {
        table.row(vec![
            connection.id.clone(),
            connection.repository.slug.clone(),
            connection.forge.clone(),
            connection.repository.repo_full_name.clone(),
            connection.created_at.clone(),
        ]);
    }
    format!("{}total: {}\n", table.render(), connections.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> ForgeConnection {
        ForgeConnection {
            id: "c-1".into(),
            forge: "github".into(),
            repository: kairos_client::types_repositories::RepositoryRef {
                id: "r-1".into(),
                slug: "payments-api".into(),
                forge: "github".into(),
                repo_full_name: "acme/payments-api".into(),
                team_id: "t-1".into(),
            },
            created_at: "2026-10-18T00:00:00Z".into(),
        }
    }

    #[test]
    fn a_connection_is_found_by_the_slug_or_the_id_of_its_repository() {
        assert!(is_of(&connection(), "payments-api"));
        assert!(is_of(&connection(), "r-1"));
        assert!(!is_of(&connection(), "web"));
    }

    #[test]
    fn add_and_rotate_print_the_url_and_the_secret_and_list_does_not() {
        let created = CreatedForgeConnection {
            connection: connection(),
            webhook_url: "https://kairos.example.com/hooks/github/acme/c-1".into(),
            webhook_secret: "s3cret".into(),
        };
        let text = secret_text("connected", "payments-api", &created);
        assert!(
            text.starts_with("Kairos connected the webhook of the repository payments-api"),
            "{text}"
        );
        assert!(
            text.contains("  Payload URL: https://kairos.example.com/hooks/github/acme/c-1\n"),
            "{text}"
        );
        assert!(text.contains("  Secret:      s3cret\n"), "{text}");
        let list = list_text(&[connection()]);
        assert!(list.contains("c-1  payments-api  github"), "{list}");
        assert!(!list.contains("s3cret"), "{list}");
        assert_eq!(list_text(&[]), "(none)\ntotal: 0\n");
    }
}
