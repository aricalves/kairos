//! `kairos metadata` and `kairos templates` (KAIROS-T-0443): the metadata
//! definitions and the templates of the organization. Writes are for
//! organization admins. A definition or a template is a slug or an id.

use kairos_client::KairosClient;
use kairos_client::types::{ListEnvelope, Pagination};
use kairos_client::types_meta::{
    CreateMetadataDefinitionRequest, CreateTemplateRequest, MetadataDefinition, Template,
    TemplateMetadataEntry, UpdateMetadataDefinitionRequest, UpdateTemplateRequest,
};

use crate::commands::entities::require_confirm;
use crate::context::{Common, client, print_json};
use crate::error::CliError;
use crate::table::{Table, print_page_footer};

/// The metadata definitions of the organization.
#[derive(clap::Subcommand, Debug)]
pub enum MetadataCommand {
    /// List the metadata definitions: slug, type, the values of an enum.
    /// One page; the last line gives the next --offset
    List(crate::commands::entities::ListArgs),
    /// Make a metadata definition (organization admins)
    Create {
        /// The name
        #[arg(long)]
        name: String,
        /// The slug
        #[arg(long)]
        slug: String,
        /// string, enum or date. It does not change later
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
    /// Delete a metadata definition (organization admins; needs --confirm).
    /// Refused while an item has a value or a template has the field
    Delete {
        /// The definition: slug or id (UUID)
        #[arg(value_name = "DEFINITION")]
        definition: String,
        /// Required: without it, the command deletes nothing
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

/// The templates of the organization.
#[derive(clap::Subcommand, Debug)]
pub enum TemplatesCommand {
    /// List the templates. One page; the last line gives the next --offset
    List(crate::commands::entities::ListArgs),
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
    /// Delete a template (organization admins; needs --confirm). Items made
    /// from it keep their content and their metadata
    Delete {
        /// The template: slug or id (UUID)
        #[arg(value_name = "TEMPLATE")]
        template: String,
        /// Required: without it, the command deletes nothing
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

/// The page size of a lookup: the server maximum.
const LOOKUP_PAGE: i64 = 200;

/// Read the pages of a list until `found` matches a row, or the rows end
/// (KAIROS-T-0593: a lookup read the first page only).
async fn find_in_pages<T, F, Fut>(
    mut page: F,
    found: impl Fn(&T) -> bool,
) -> Result<Option<T>, CliError>
where
    F: FnMut(Pagination) -> Fut,
    Fut: std::future::Future<Output = Result<ListEnvelope<T>, kairos_client::Error>>,
    T: utoipa::ToSchema,
{
    let mut offset = 0;
    loop {
        let envelope = page(Pagination {
            limit: Some(LOOKUP_PAGE),
            offset: Some(offset),
        })
        .await?;
        let shown = i64::try_from(envelope.items.len()).unwrap_or(i64::MAX);
        if let Some(row) = envelope.items.into_iter().find(|row| found(row)) {
            return Ok(Some(row));
        }
        offset = offset.saturating_add(shown);
        if shown == 0 || offset >= envelope.total {
            return Ok(None);
        }
    }
}

async fn definition(
    client: &KairosClient,
    reference: &str,
) -> Result<MetadataDefinition, CliError> {
    find_in_pages(
        |page| client.list_metadata_definitions(page),
        |d: &MetadataDefinition| d.id == reference || d.slug == reference,
    )
    .await?
    .ok_or_else(|| {
        CliError::Failure(format!(
            "No metadata definition has the slug or the id {reference:?}. `kairos metadata \
             list` gives them."
        ))
    })
}

async fn template(client: &KairosClient, reference: &str) -> Result<Template, CliError> {
    find_in_pages(
        |page| client.list_templates(page),
        |t: &Template| t.id == reference || t.slug == reference,
    )
    .await?
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
            Self::List(args) => {
                let client = client(&args.common)?;
                let all = client.list_metadata_definitions(args.page()).await?;
                if args.common.json {
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
                print_page_footer(all.total, all.limit, all.offset, all.items.len());
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
                if name.is_none() && slug.is_none() && options.is_empty() && entity_types.is_empty()
                {
                    return Err(CliError::Failure(
                        "The command has no change. Use --name, --slug, --option or --entity-type."
                            .to_string(),
                    ));
                }
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
                confirm,
                common,
            } => {
                require_confirm(confirm, &format!("the metadata definition {reference}"))?;
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
            Self::List(args) => {
                let client = client(&args.common)?;
                let all = client.list_templates(args.page()).await?;
                if args.common.json {
                    return print_json(&all);
                }
                let mut table = Table::new(&["SLUG", "NAME", "ID"]);
                for t in &all.items {
                    table.row(vec![t.slug.clone(), t.name.clone(), t.id.clone()]);
                }
                print!("{}", table.render());
                print_page_footer(all.total, all.limit, all.offset, all.items.len());
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
                if name.is_none() && slug.is_none() && content.is_none() && raw.is_empty() {
                    return Err(CliError::Failure(
                        "The command has no change. Use --name, --slug, --content or --field."
                            .to_string(),
                    ));
                }
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
                confirm,
                common,
            } => {
                require_confirm(confirm, &format!("the template {reference}"))?;
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

    fn fixture(n: usize) -> Template {
        Template {
            id: format!("id-{n}"),
            name: format!("Template {n}"),
            slug: format!("slug-{n}"),
            content: String::new(),
            is_system_default: false,
            created_at: "2026-10-17T00:00:00Z".into(),
            updated_at: "2026-10-17T00:00:00Z".into(),
        }
    }

    /// A server page of `limit` rows from 450 fixtures, with the offsets
    /// that the lookup asked for.
    async fn fixture_page(
        asked: &std::sync::Mutex<Vec<i64>>,
        page: Pagination,
    ) -> Result<ListEnvelope<Template>, kairos_client::Error> {
        let offset = page.offset.unwrap_or(0);
        let limit = page.limit.unwrap_or(50);
        asked.lock().unwrap().push(offset);
        let items = (0..450usize)
            .skip(usize::try_from(offset).unwrap())
            .take(usize::try_from(limit).unwrap())
            .map(fixture)
            .collect();
        Ok(ListEnvelope {
            items,
            total: 450,
            limit,
            offset,
        })
    }

    /// KAIROS-T-0593: the lookup by slug reads the next page when the
    /// first page does not have the slug, and stops at the last page.
    #[tokio::test]
    async fn a_lookup_reads_each_page_until_it_finds_the_slug() {
        let asked = std::sync::Mutex::new(Vec::new());
        let found = find_in_pages(
            |page| fixture_page(&asked, page),
            |t: &Template| t.slug == "slug-401",
        )
        .await
        .unwrap();
        assert_eq!(found.map(|t| t.id), Some("id-401".to_string()));
        assert_eq!(*asked.lock().unwrap(), vec![0, 200, 400]);

        asked.lock().unwrap().clear();
        let missing = find_in_pages(
            |page| fixture_page(&asked, page),
            |t: &Template| t.slug == "slug-9999",
        )
        .await
        .unwrap();
        assert!(missing.is_none());
        assert_eq!(*asked.lock().unwrap(), vec![0, 200, 400]);
    }
}
