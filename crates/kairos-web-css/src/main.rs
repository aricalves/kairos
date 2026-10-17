//! KAIROS-T-0326: the trunk `pre_build` hook of `kairos-web` runs this to
//! write the stylesheet that `index.html` links as a normal stylesheet.
//! The page then has the styles at its first paint, with no flash (the
//! Aurora README, "Linked stylesheet").
//!
//! KAIROS-T-0605: it writes two files.
//!
//! - `kairos.css`: the Aurora stylesheet, then `app.css` (the Kairos-only
//!   rules). One file, so one request blocks the first paint.
//! - `hyper-<hash>.css`: the opt-in hyper theme of Aurora (KAIROS-T-0397).
//!   `index.html` copies it to `dist` and loads it only when the person
//!   chooses Hyper. The server sends each asset as immutable, so the name
//!   holds a hash of the content. A test checks that `index.html` names
//!   the current file.
//!
//! Usage: `kairos-web-css <dir>`. Trunk runs it with the source directory
//! of the web crate in `TRUNK_SOURCE_DIR`; a relative `<dir>` is read from
//! there.

use std::path::{Path, PathBuf};

/// The Kairos-only rules (tokens only). They come after Aurora.
const APP_CSS: &str = include_str!("../../kairos-web/app.css");

/// The one stylesheet of the page: Aurora, then the Kairos rules.
fn kairos_css() -> String {
    format!("{}\n{}", aurora_dark::AURORA_CSS, APP_CSS)
}

/// The name of the hyper file: `hyper-<16 hex digits>.css`, from a 64-bit
/// FNV-1a hash of the content.
fn hyper_file_name() -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in aurora_dark::HYPER_CSS.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("hyper-{hash:016x}.css")
}

fn write(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("kairos.css"), kairos_css())?;
    let hyper = hyper_file_name();
    // An old hyper file stays out, so a stale name in index.html fails the
    // trunk build.
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("hyper") && name.ends_with(".css") && name != hyper {
            std::fs::remove_file(dir.join(name.as_ref()))?;
        }
    }
    std::fs::write(dir.join(&hyper), aurora_dark::HYPER_CSS)?;
    Ok(())
}

fn main() -> std::io::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "style".into()));
    let dir = match std::env::var_os("TRUNK_SOURCE_DIR") {
        Some(source) if dir.is_relative() => PathBuf::from(source).join(dir),
        _ => dir,
    };
    write(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX_HTML: &str = include_str!("../../kairos-web/index.html");

    /// KAIROS-T-0605: Aurora comes first, then the Kairos rules.
    #[test]
    fn the_stylesheet_is_aurora_then_app_css() {
        let css = kairos_css();
        assert!(css.starts_with(aurora_dark::AURORA_CSS));
        assert!(css.ends_with(APP_CSS));
        assert!(!css.contains("--aurora-hyper-loaded"), "hyper stays out");
    }

    /// KAIROS-T-0605: index.html copies the current hyper file and its
    /// script loads that name. After an Aurora upgrade, write the new name
    /// in index.html.
    #[test]
    fn index_html_names_the_current_hyper_file() {
        let name = hyper_file_name();
        assert!(
            INDEX_HTML.contains(&format!(
                r#"<link data-trunk rel="copy-file" href="style/{name}" />"#
            )),
            "index.html copies style/{name}"
        );
        assert!(
            INDEX_HTML.contains(&format!(r#"var H = "/{name}";"#)),
            "the script of index.html loads /{name}"
        );
    }

    #[test]
    fn the_hook_writes_both_files_and_removes_an_old_hyper_file() {
        let dir = std::env::temp_dir().join(format!("kairos-web-css-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hyper-0000000000000000.css"), "old").unwrap();
        write(&dir).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec![hyper_file_name(), "kairos.css".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dir.join(hyper_file_name())).unwrap(),
            aurora_dark::HYPER_CSS
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
