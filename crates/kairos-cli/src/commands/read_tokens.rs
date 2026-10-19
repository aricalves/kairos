//! `kairos admin read-tokens` (KAIROS-T-0592): the read tokens of the
//! organization, one for each forge host (`/api/org/read-tokens`,
//! KAIROS-T-0431). The builder of the code index gives the token to git for
//! each repository on the host that has no read token of its own.
//!
//! `set` reads the token from standard input, or asks for it on the
//! terminal with no echo, never from an argument. Kairos keeps the token
//! encrypted, and no read gives it back: no command prints it.

use kairos_client::types_code_index::OrgReadToken;

use crate::commands::entities::require_confirm;
use crate::context::{Common, client, print_json};
use crate::error::CliError;
use crate::listing::print_whole;
use crate::table::Table;

/// The read tokens of the organization (an organization admin).
#[derive(clap::Subcommand, Debug)]
pub enum ReadTokensCommand {
    /// List the hosts that have a read token: who set it and when. The list
    /// shows no token
    List {
        #[command(flatten)]
        common: Common,
    },
    /// Set or replace the read token of a host, for example github.com. The
    /// command reads the token from standard input, or asks for it on the
    /// terminal. It does not take the token as an argument
    Set {
        /// The forge host, for example github.com (with :port when the port
        /// is not the default)
        host: String,
        #[command(flatten)]
        common: Common,
    },
    /// Remove the read token of a host (requires --confirm)
    Remove {
        /// The forge host, for example github.com
        host: String,
        /// Remove the token
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

impl ReadTokensCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List { common } => {
                let client = client(&common)?;
                let tokens = client.org_read_tokens().await?;
                if common.json {
                    return print_whole(&tokens);
                }
                print!("{}", tokens_text(&tokens));
                Ok(())
            }
            Self::Set { host, common } => {
                let token = crate::password::read_secret(
                    &format!("Read token of the organization for {host}: "),
                    "token",
                )?;
                let client = client(&common)?;
                let set = client.set_org_read_token(&host, &token).await?;
                if common.json {
                    return print_json(&set);
                }
                println!(
                    "Kairos keeps the read token of the organization for {}, encrypted.",
                    set.host
                );
                Ok(())
            }
            Self::Remove {
                host,
                confirm,
                common,
            } => {
                require_confirm(confirm, &format!("the read token for {host}"))?;
                let client = client(&common)?;
                let left = client.remove_org_read_token(&host).await?;
                if common.json {
                    return print_whole(&left);
                }
                println!("Kairos removed the read token of the organization for {host}.");
                Ok(())
            }
        }
    }
}

/// The text of `list`. Pure.
fn tokens_text(tokens: &[OrgReadToken]) -> String {
    if tokens.is_empty() {
        return "(none)\ntotal: 0\n".to_string();
    }
    let mut table = Table::new(&["HOST", "SET_BY", "SET_AT"]);
    for token in tokens {
        table.row(vec![
            token.host.clone(),
            token
                .set_by_name
                .clone()
                .unwrap_or_else(|| token.set_by.clone()),
            token.set_at.clone(),
        ]);
    }
    format!("{}total: {}\n", table.render(), tokens.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_names_the_host_and_who_set_the_token() {
        let token = OrgReadToken {
            host: "github.com".into(),
            set_by: "u-1".into(),
            set_by_name: Some("Robin".into()),
            set_at: "2026-10-18T00:00:00Z".into(),
        };
        let text = tokens_text(&[token]);
        assert!(
            text.contains("github.com  Robin   2026-10-18T00:00:00Z"),
            "{text}"
        );
        assert!(text.ends_with("total: 1\n"), "{text}");
        assert_eq!(tokens_text(&[]), "(none)\ntotal: 0\n");
    }
}
