//! `kairos pages` — the published site of a repository (KAIROS-T-0408,
//! KAIROS-D-0012). CI builds the site with the base path that Kairos gives,
//! then publishes it:
//!
//! ```text
//! BASE=$(kairos pages base-path --repo payments)
//! mdbook build -d dist           # with site-url = $BASE
//! kairos pages publish dist --repo payments
//! ```
//!
//! `publish` takes a folder (it makes the `.tar.gz`) or a `.tar.gz`, and
//! sends its MD5 as `Content-MD5`, so a body that changes on the way is
//! refused. `on` and `off` change the site switch of the repository.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use kairos_client::types_repositories::UpdateRepositoryRequest;
use md5::{Digest as _, Md5};

use crate::commands::entities::require_confirm;
use crate::context::{Common, client, print_json};
use crate::error::CliError;

/// Operations on the published site of a repository.
#[derive(clap::Subcommand, Debug)]
pub enum PagesCommand {
    /// Publish a built site: a folder, or a .tar.gz with index.html at its
    /// root. The site must be on, and you need the capability pages.
    Publish {
        /// The build folder, or a .tar.gz of it
        path: PathBuf,
        /// Repository slug (or UUID)
        #[arg(long, value_name = "REPOSITORY")]
        repo: String,
        #[command(flatten)]
        common: Common,
    },
    /// Print the base path of the site: give it to the build as the path
    /// where the site lives
    #[command(name = "base-path")]
    BasePath {
        /// Repository slug (or UUID)
        #[arg(long, value_name = "REPOSITORY")]
        repo: String,
        #[command(flatten)]
        common: Common,
    },
    /// Unpublish the site. Kairos deletes its files; the history stays.
    Unpublish {
        /// Repository slug (or UUID)
        #[arg(long, value_name = "REPOSITORY")]
        repo: String,
        /// Required: without it, the command unpublishes nothing and exits 1
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Turn the site of a repository on
    On {
        /// Repository slug (or UUID)
        #[arg(long, value_name = "REPOSITORY")]
        repo: String,
        #[command(flatten)]
        common: Common,
    },
    /// Turn the site of a repository off: Kairos does not serve it, and a
    /// publish is refused
    Off {
        /// Repository slug (or UUID)
        #[arg(long, value_name = "REPOSITORY")]
        repo: String,
        #[command(flatten)]
        common: Common,
    },
}

/// The `.tar.gz` of a folder: each file and folder under it, at the root of
/// the archive. Symlinks stay symlinks, so that the server refuses them and
/// names them.
pub(crate) fn tar_folder(dir: &Path) -> Result<Vec<u8>, CliError> {
    let fail = |e: std::io::Error| {
        CliError::Failure(format!("Cannot make the archive of {}: {e}", dir.display()))
    };
    let mut out = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        builder.follow_symlinks(false);
        builder.append_dir_all(".", dir).map_err(fail)?;
        builder.into_inner().map_err(fail)?.finish().map_err(fail)?;
    }
    Ok(out)
}

/// The bytes to send: the archive of a folder, or the file itself.
fn archive_of(path: &Path) -> Result<Vec<u8>, CliError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| CliError::Failure(format!("Cannot read {}: {e}", path.display())))?;
    if meta.is_dir() {
        if !path.join("index.html").is_file() {
            return Err(CliError::Failure(format!(
                "The folder {} has no index.html. Give the build folder of the site.",
                path.display()
            )));
        }
        return tar_folder(path);
    }
    std::fs::read(path)
        .map_err(|e| CliError::Failure(format!("Cannot read {}: {e}", path.display())))
}

/// The MD5 of `bytes` in base64, for `Content-MD5`.
pub(crate) fn content_md5(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Md5::digest(bytes))
}

fn switch(on: bool) -> UpdateRepositoryRequest {
    UpdateRepositoryRequest {
        site_enabled: Some(on),
        ..UpdateRepositoryRequest::default()
    }
}

impl PagesCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            PagesCommand::Publish { path, repo, common } => {
                let archive = archive_of(&path)?;
                let md5 = content_md5(&archive);
                let client = client(&common)?;
                let published = client.publish_site(&repo, archive, &md5).await?;
                if common.json {
                    return print_json(&published);
                }
                println!(
                    "Published the site of {repo} at {} ({} entries, {} bytes, MD5 {}).",
                    published.base_path, published.files, published.size, published.md5
                );
                Ok(())
            }
            PagesCommand::BasePath { repo, common } => {
                let client = client(&common)?;
                let repository = client.get_repository(&repo).await?;
                if common.json {
                    return print_json(&repository.repository.site);
                }
                println!("{}", repository.repository.site.base_path);
                Ok(())
            }
            PagesCommand::Unpublish {
                repo,
                confirm,
                common,
            } => {
                require_confirm(confirm, &format!("the site of {repo}"))?;
                let client = client(&common)?;
                client.unpublish_site(&repo).await?;
                if common.json {
                    return print_json(&serde_json::json!({ "unpublished": repo }));
                }
                println!("Unpublished the site of {repo}.");
                Ok(())
            }
            PagesCommand::On { repo, common } => set_site(&repo, true, &common).await,
            PagesCommand::Off { repo, common } => set_site(&repo, false, &common).await,
        }
    }
}

async fn set_site(repo: &str, on: bool, common: &Common) -> Result<(), CliError> {
    let client = client(common)?;
    let repository = client.update_repository(repo, &switch(on)).await?;
    if common.json {
        return print_json(&repository.site);
    }
    let state = if on { "on" } else { "off" };
    println!(
        "The site of {} is {state}. Its base path: {}",
        repository.slug, repository.site.base_path
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    /// The archive of a folder has its files at the root, with their bytes.
    #[test]
    fn a_folder_becomes_a_tar_gz_with_its_files_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>hi</h1>").unwrap();
        std::fs::create_dir(dir.path().join("guide")).unwrap();
        std::fs::write(dir.path().join("guide/install.html"), b"install").unwrap();
        let bytes = archive_of(dir.path()).unwrap();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
        let mut files = Vec::new();
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.header().entry_type().is_file() {
                let path = entry.path().unwrap().to_string_lossy().to_string();
                let mut data = String::new();
                entry.read_to_string(&mut data).unwrap();
                files.push((path, data));
            }
        }
        files.sort();
        assert_eq!(
            files,
            vec![
                ("guide/install.html".to_string(), "install".to_string()),
                ("index.html".to_string(), "<h1>hi</h1>".to_string()),
            ]
        );
    }

    #[test]
    fn a_folder_with_no_index_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("other.html"), b"x").unwrap();
        let err = archive_of(dir.path()).unwrap_err();
        assert!(err.to_string().contains("no index.html"), "{err}");
    }

    #[test]
    fn content_md5_is_base64_of_the_digest() {
        // RFC 1864: the MD5 of "" is 1B2M2Y8AsgTpgAmY7PhCfg==.
        assert_eq!(content_md5(b""), "1B2M2Y8AsgTpgAmY7PhCfg==");
    }
}
