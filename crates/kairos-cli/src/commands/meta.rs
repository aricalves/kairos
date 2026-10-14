//! `kairos metadata` and `kairos templates` (KAIROS-T-0443): the metadata
//! definitions and the templates of the organization. Writes are for
//! organization admins. A definition or a template is a slug or an id.

use kairos_client::KairosClient;
use kairos_client::types::Pagination;
use kairos_client::types_meta::{
    CreateMetadataDefinitionRequest, CreateTemplateRequest, MetadataDefinition, Template,
    TemplateMetadataEntry, UpdateMetadataDefinitionRequest, UpdateTemplateRequest,
};

use crate::context::{Common, client, print_json};
use crate::error::CliError;
use crate::table::Table;

/// The metadata definitions of the organization.
#[derive(clap::Subcommand, Debug)]
pub enum MetadataCommand {
    /// List the metadata definitions: slug, type, the values of an enum
    List {
        #[command(flatten)]
        common: Common,
    },
    /// Make a metadata definition (organization admins)
    Create {
        /// The name
        #[arg(long)]
        name: String,
        /// The slug
        #[arg(long)]
        slug: String,
        /// text, number, date, enum or boolean. It does not change later
        #[arg(long = "type", value_name = "TYPE")]
        field_type: String,
        /// A value of an enum; repeat it for each value, in the order of the
        /// scale
        #[arg(long = "option", value_name = "VALUE")]
        options: Vec<String>,
        /// An entity type of the definition (strategy, initiative, task,
        /// document, adr); repeat it. None: each type
        #[arg(long = "entity-type", value_name = "TYPE")]
        entity_types: Vec<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Change a metadata definition (organization admins). --option and
    /// --entity-type replace the lists
    Update {
        /// The definition: slug or id (UUID)
        #[arg(value_name = "DEFINITION")]
        definition: String,
        /// The new name
        #[arg(long)]
        name: Option<String>,
        /// The new slug
        #[arg(long)]
        slug: Option<String>,
        /// A value of an enum; repeat it. It replaces the list
        #[arg(long = "option", value_name = "VALUE")]
        options: Vec<String>,
        /// An entity type; repeat it. It replaces the list
        #[arg(long = "entity-type", value_name = "TYPE")]
        entity_types: Vec<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Delete a metadata definition (organization admins). Refused while an
    /// item has a value or a template has the field
    Delete {
        /// The definition: slug or id (UUID)
        #[arg(value_name = "DEFINITION")]
        definition: String,
        #[command(flatten)]
        common: Common,
    },
}

/// The templates of the organization.
#[derive(clap::Subcommand, Debug)]
pub enum TemplatesCommand {
    /// List the templates
    List {
        #[command(flatten)]
        common: Common,
    },
    /// Make a template (organization admins)
    Create {
        /// The name
        #[arg(long)]
        name: String,
        /// The slug
        #[arg(long)]
        slug: String,
        /// The markdown content of a new item of the template
        #[arg(long, default_value = "")]
        content: String,
        /// A metadata field as SLUG, SLUG=DEFAULT, SLUG! or SLUG=DEFAULT!
        /// (! makes it required); repeat it
        #[arg(long = "field", value_name = "FIELD")]
        fields: Vec<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Change a template (organization admins). --field replaces the list
    Update {
        /// The template: slug or id (UUID)
        #[arg(value_name = "TEMPLATE")]
        template: String,
        /// The new name
        #[arg(long)]
        name: Option<String>,
        /// The new slug
        #[arg(long)]
        slug: Option<String>,
        /// The new content
        #[arg(long)]
        content: Option<String>,
        /// A metadata field as SLUG, SLUG=DEFAULT, SLUG! or SLUG=DEFAULT!;
        /// repeat it. It replaces the list
        #[arg(long = "field", value_name = "FIELD")]
        fields: Vec<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Delete a template (organization admins). Items made from it keep
    /// their content and their metadata
    Delete {
        /// The template: slug or id (UUID)
        #[arg(value_name = "TEMPLATE")]
        template: String,
        #[command(flatten)]
        common: Common,
    },
}

const ALL: Pagination = Pagination {
    limit: Some(200),
    offset: None,
};

async fn definition(
    client: &KairosClient,
    reference: &str,
) -> Result<MetadataDefinition, CliError> {
    let all = client.list_metadata_definitions(ALL).await?.items;
    all.into_iter()
        .find(|d| d.id == reference || d.slug == reference)
        .ok_or_else(|| {
            CliError::Failure(format!(
                "No metadata definition has the slug or the id {reference:?}. `kairos metadata \
                 list` gives them."
            ))
        })
}

async fn template(client: &KairosClient, reference: &str) -> Result<Template, CliError> {
    let all = client.list_templates(ALL).await?.items;
    all.into_iter()
        .find(|t| t.id == reference || t.slug == reference)
        .ok_or_else(|| {
            CliError::Failure(format!(
                "No template has the slug or the id {reference:?}. `kairos templates list` gives \
                 them."
            ))
        })
}

/// A `--field` value: `slug`, `slug=default`, with `!` at the end for a
/// required field. Pure.
fn field(raw: &str) -> Result<TemplateMetadataEntry, CliError> {
    let raw = raw.trim();
    let (raw, required) = match raw.strip_suffix('!') {
        Some(rest) => (rest, true),
        None => (raw, false),
    };
    let (slug, default_value) = match raw.split_once('=') {
        Some((slug, value)) => (slug.trim(), Some(value.trim().to_string())),
        None => (raw, None),
    };
    if slug.is_empty() {
        return Err(CliError::Failure(format!(
            "The field {raw:?} has no slug. Write SLUG, SLUG=DEFAULT, SLUG! or SLUG=DEFAULT!."
        )));
    }
    Ok(TemplateMetadataEntry {
        definition_slug: slug.to_string(),
        default_value,
        required,
    })
}

fn fields(raw: &[String]) -> Result<Vec<TemplateMetadataEntry>, CliError> {
    raw.iter().map(|f| field(f)).collect()
}

fn definition_line(d: &MetadataDefinition) -> String {
    format!(
        "The metadata definition {} ({}): {}, type {}.",
        d.slug, d.id, d.name, d.field_type
    )
}

impl MetadataCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List { common } => {
                let client = client(&common)?;
                let all = client.list_metadata_definitions(ALL).await?;
                if common.json {
                    return print_json(&all);
                }
                let mut table = Table::new(&["SLUG", "NAME", "TYPE", "VALUES", "ENTITY TYPES"]);
                for d in &all.items {
                    table.row(vec![
                        d.slug.clone(),
                        d.name.clone(),
                        d.field_type.clone(),
                        d.enum_options.join(", "),
                        if d.entity_types.is_empty() {
                            "each".into()
                        } else {
                            d.entity_types.join(", ")
                        },
                    ]);
                }
                print!("{}", table.render());
                Ok(())
            }
            Self::Create {
                name,
                slug,
                field_type,
                options,
                entity_types,
                common,
            } => {
                let client = client(&common)?;
                let made = client
                    .create_metadata_definition(&CreateMetadataDefinitionRequest {
                        name,
                        slug,
                        field_type,
                        enum_options: options,
                        entity_types,
                    })
                    .await?;
                if common.json {
                    return print_json(&made);
                }
                println!("{}", definition_line(&made));
                Ok(())
            }
            Self::Update {
                definition: reference,
                name,
                slug,
                options,
                entity_types,
                common,
            } => {
                let client = client(&common)?;
                let current = definition(&client, &reference).await?;
                let changed = client
                    .update_metadata_definition(
                        &current.id,
                        &UpdateMetadataDefinitionRequest {
                            name,
                            slug,
                            enum_options: (!options.is_empty()).then_some(options),
                            entity_types: (!entity_types.is_empty()).then_some(entity_types),
                        },
                    )
                    .await?;
                if common.json {
                    return print_json(&changed);
                }
                println!("{}", definition_line(&changed));
                Ok(())
            }
            Self::Delete {
                definition: reference,
                common,
            } => {
                let client = client(&common)?;
                let current = definition(&client, &reference).await?;
                let deleted = client.delete_metadata_definition(&current.id).await?;
                if common.json {
                    return print_json(&deleted);
                }
                println!("Deleted the metadata definition {}.", current.slug);
                Ok(())
            }
        }
    }
}

impl TemplatesCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List { common } => {
                let client = client(&common)?;
                let all = client.list_templates(ALL).await?;
                if common.json {
                    return print_json(&all);
                }
                let mut table = Table::new(&["SLUG", "NAME", "ID"]);
                for t in &all.items {
                    table.row(vec![t.slug.clone(), t.name.clone(), t.id.clone()]);
                }
                print!("{}", table.render());
                Ok(())
            }
            Self::Create {
                name,
                slug,
                content,
                fields: raw,
                common,
            } => {
                let client = client(&common)?;
                let made = client
                    .create_template(&CreateTemplateRequest {
                        name,
                        slug,
                        content,
                        metadata: fields(&raw)?,
                    })
                    .await?;
                if common.json {
                    return print_json(&made);
                }
                println!("Made the template {} ({}).", made.slug, made.id);
                Ok(())
            }
            Self::Update {
                template: reference,
                name,
                slug,
                content,
                fields: raw,
                common,
            } => {
                let client = client(&common)?;
                let current = template(&client, &reference).await?;
                let metadata = if raw.is_empty() {
                    None
                } else {
                    Some(fields(&raw)?)
                };
                let changed = client
                    .update_template(
                        &current.id,
                        &UpdateTemplateRequest {
                            name,
                            slug,
                            content,
                            metadata,
                        },
                    )
                    .await?;
                if common.json {
                    return print_json(&changed);
                }
                println!("Changed the template {} ({}).", changed.slug, changed.id);
                Ok(())
            }
            Self::Delete {
                template: reference,
                common,
            } => {
                let client = client(&common)?;
                let current = template(&client, &reference).await?;
                let deleted = client.delete_template(&current.id).await?;
                if common.json {
                    return print_json(&deleted);
                }
                println!("Deleted the template {}.", current.slug);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_reads_its_slug_default_and_required_mark() {
        assert_eq!(
            field("priority").unwrap(),
            TemplateMetadataEntry {
                definition_slug: "priority".into(),
                default_value: None,
                required: false,
            }
        );
        assert_eq!(
            field("priority=high!").unwrap(),
            TemplateMetadataEntry {
                definition_slug: "priority".into(),
                default_value: Some("high".into()),
                required: true,
            }
        );
        assert!(field("=high").is_err());
    }
}
