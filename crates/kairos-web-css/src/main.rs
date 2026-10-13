//! KAIROS-T-0326: the trunk `pre_build` hook of `kairos-web` runs this to
//! write `style/aurora.css`, which `index.html` links as a normal
//! stylesheet. The page then has the Aurora styles at its first paint,
//! with no flash (the Aurora README, "Linked stylesheet").
//!
//! KAIROS-T-0397: it writes `style/hyper.css` too, the opt-in hyper theme
//! of Aurora. `index.html` links it after `aurora.css`.
//!
//! Usage: `kairos-web-css <dir>`. Trunk runs it with the source directory
//! of the web crate in `TRUNK_SOURCE_DIR`; a relative `<dir>` is read from
//! there.

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "style".into()));
    let dir = match std::env::var_os("TRUNK_SOURCE_DIR") {
        Some(source) if dir.is_relative() => PathBuf::from(source).join(dir),
        _ => dir,
    };
    aurora_dark::write_css(&dir)?;
    aurora_dark::write_hyper_css(&dir)?;
    Ok(())
}
