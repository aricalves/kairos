//! The archive of a site upload (KAIROS-T-0406, KAIROS-D-0012 "Archive
//! rules" and "Upload limits").
//!
//! [`unpack`] reads a `.tar.gz` and writes its files to a folder, with the
//! manifest of each file. It refuses, and names the path or the limit:
//!
//! - an entry that is not a file or a folder (a symlink, a hard link, a
//!   device, a FIFO);
//! - an absolute path, a `..` part, a backslash, a path that is not UTF-8,
//!   or a path longer than [`MAX_PATH`] bytes;
//! - two entries with one path;
//! - more entries than the limit, or more bytes of files than the limit:
//!   it stops at that entry, so a compressed bomb costs no more than the
//!   limit;
//! - an archive with no `index.html` at its root.
//!
//! A leading `./` is accepted and dropped.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The longest path in a site, in bytes.
pub const MAX_PATH: usize = 1024;

/// The limits of one upload.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Bytes of the files.
    pub max_size: u64,
    /// Entries (files and folders).
    pub max_files: u64,
}

/// One file of the site, written to the folder of [`unpack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpackedFile {
    /// The path in the site, relative, `/`-separated.
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub content_type: String,
}

/// The result of [`unpack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    pub files: Vec<UnpackedFile>,
    /// Bytes of the files.
    pub unpacked_size: u64,
    /// Entries of the archive (files and folders).
    pub entries: u64,
}

/// Why an archive is refused. The texts name the path or the limit.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArchiveError {
    #[error("The archive is not a gzip tar file: {0}. Send a .tar.gz of the built site.")]
    NotATarGz(String),
    #[error(
        "The entry {0:?} is a symlink, a hard link, a device or a FIFO. An archive can have files and folders only."
    )]
    NotAFile(String),
    #[error(
        "The path {0:?} is absolute. Each path in the archive must be relative to the root of the site."
    )]
    Absolute(String),
    #[error("The path {0:?} has a \"..\" part. A path cannot go above the root of the site.")]
    DotDot(String),
    #[error("The path {0:?} has a backslash. Use \"/\" between the parts of a path.")]
    Backslash(String),
    #[error("A path in the archive is not UTF-8. Each path must be UTF-8.")]
    NotUtf8,
    #[error("The path {path:?} has {len} bytes. The limit is {MAX_PATH} bytes.")]
    PathTooLong { path: String, len: usize },
    #[error("The archive has the path {0:?} 2 times. Each path can occur 1 time.")]
    Duplicate(String),
    #[error("The archive has more than {limit} entries (files and folders). The limit is {limit}.")]
    TooManyEntries { limit: u64 },
    #[error(
        "The files of the archive are larger than {limit} bytes (at {path:?}). The limit is {limit} bytes."
    )]
    TooLarge { limit: u64, path: String },
    #[error(
        "The archive has no index.html at its root. Make the archive from inside the build folder, so that index.html is at the root."
    )]
    NoIndex,
    #[error("Kairos cannot write the files of the archive: {0}")]
    Io(String),
}

/// The checked path of an entry, or why it is refused. `None`: the root
/// folder itself (`./`), which is not an entry of the site.
fn site_path(raw: &[u8]) -> Result<Option<String>, ArchiveError> {
    let text = std::str::from_utf8(raw).map_err(|_| ArchiveError::NotUtf8)?;
    if text.contains('\0') {
        return Err(ArchiveError::NotUtf8);
    }
    if text.contains('\\') {
        return Err(ArchiveError::Backslash(text.to_string()));
    }
    if text.starts_with('/') {
        return Err(ArchiveError::Absolute(text.to_string()));
    }
    if text.len() > MAX_PATH {
        return Err(ArchiveError::PathTooLong {
            path: text.chars().take(80).collect(),
            len: text.len(),
        });
    }
    let mut parts = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(ArchiveError::DotDot(text.to_string())),
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts.join("/")))
}

/// The content type of a path, from its extension.
pub fn content_type(path: &str) -> String {
    let guess = mime_guess::from_path(path).first_or_octet_stream();
    match (guess.type_(), guess.subtype().as_str()) {
        (mime_guess::mime::TEXT, _) | (mime_guess::mime::APPLICATION, "javascript" | "json") => {
            format!("{guess}; charset=utf-8")
        }
        _ => guess.to_string(),
    }
}

/// Read the `.tar.gz` at `archive` and write its files under `out`.
pub fn unpack(archive: &Path, out: &Path, limits: Limits) -> Result<Unpacked, ArchiveError> {
    let io = |e: io::Error| ArchiveError::Io(e.to_string());
    let file = File::open(archive).map_err(io)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = tar
        .entries()
        .map_err(|e| ArchiveError::NotATarGz(e.to_string()))?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut files = Vec::new();
    let mut unpacked_size: u64 = 0;
    let mut count: u64 = 0;
    for entry in entries {
        let mut entry = entry.map_err(|e| ArchiveError::NotATarGz(e.to_string()))?;
        let raw = entry.path_bytes().into_owned();
        let kind = entry.header().entry_type();
        let path = match site_path(&raw)? {
            Some(path) => path,
            None if kind.is_dir() => continue,
            None => return Err(ArchiveError::NotAFile(String::from_utf8_lossy(&raw).into())),
        };
        if !(kind.is_file() || kind.is_dir()) {
            return Err(ArchiveError::NotAFile(path));
        }
        count += 1;
        if count > limits.max_files {
            return Err(ArchiveError::TooManyEntries {
                limit: limits.max_files,
            });
        }
        if !seen.insert(path.clone()) {
            return Err(ArchiveError::Duplicate(path));
        }
        let target: PathBuf = out.join(&path);
        if kind.is_dir() {
            std::fs::create_dir_all(&target).map_err(io)?;
            continue;
        }
        let declared = entry.header().size().map_err(io)?;
        if unpacked_size.saturating_add(declared) > limits.max_size {
            return Err(ArchiveError::TooLarge {
                limit: limits.max_size,
                path,
            });
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let mut writer = File::create(&target).map_err(io)?;
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        let mut buffer = [0u8; 64 * 1024];
        // Read no more than the limit allows, whatever the header says.
        let mut reader = (&mut entry).take(limits.max_size - unpacked_size + 1);
        loop {
            let n = reader.read(&mut buffer).map_err(io)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            if unpacked_size + size > limits.max_size {
                return Err(ArchiveError::TooLarge {
                    limit: limits.max_size,
                    path,
                });
            }
            hasher.update(&buffer[..n]);
            writer.write_all(&buffer[..n]).map_err(io)?;
        }
        unpacked_size += size;
        files.push(UnpackedFile {
            content_type: content_type(&path),
            path,
            size,
            sha256: hex(&hasher.finalize()),
        });
    }
    if !files.iter().any(|f| f.path == "index.html") {
        return Err(ArchiveError::NoIndex);
    }
    Ok(Unpacked {
        files,
        unpacked_size,
        entries: count,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    const LIMITS: Limits = Limits {
        max_size: 1 << 20,
        max_files: 100,
    };

    /// One entry of a test archive.
    pub(crate) enum Entry<'a> {
        File(&'a str, &'a [u8]),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
        HardLink(&'a str, &'a str),
        Fifo(&'a str),
        /// A file whose header says `declared` bytes.
        Sized(&'a str, u64),
    }

    /// A `.tar.gz` of `entries`, with each path written as given (the
    /// `tar` builder would refuse the bad ones).
    pub(crate) fn archive(entries: &[Entry]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let gz = GzEncoder::new(&mut out, Compression::fast());
            let mut builder = tar::Builder::new(gz);
            for entry in entries {
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o644);
                let (path, kind, data, link): (&str, tar::EntryType, Vec<u8>, Option<&str>) =
                    match entry {
                        Entry::File(p, d) => (p, tar::EntryType::Regular, d.to_vec(), None),
                        Entry::Dir(p) => (p, tar::EntryType::Directory, vec![], None),
                        Entry::Symlink(p, t) => (p, tar::EntryType::Symlink, vec![], Some(t)),
                        Entry::HardLink(p, t) => (p, tar::EntryType::Link, vec![], Some(t)),
                        Entry::Fifo(p) => (p, tar::EntryType::Fifo, vec![], None),
                        Entry::Sized(p, n) => {
                            (p, tar::EntryType::Regular, vec![0u8; *n as usize], None)
                        }
                    };
                header.set_entry_type(kind);
                header.set_size(data.len() as u64);
                if path.len() > 100 {
                    // A long path needs the GNU long-name entry of the builder.
                    builder
                        .append_data(&mut header, path, data.as_slice())
                        .unwrap();
                    continue;
                }
                {
                    let name = &mut header.as_old_mut().name;
                    name.fill(0);
                    name[..path.len()].copy_from_slice(path.as_bytes());
                }
                if let Some(link) = link {
                    header.set_link_name(link).unwrap();
                }
                header.set_cksum();
                builder.append(&header, data.as_slice()).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        out
    }

    fn run(bytes: &[u8], limits: Limits) -> Result<Unpacked, ArchiveError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("site.tar.gz");
        std::fs::write(&path, bytes).unwrap();
        unpack(&path, &dir.path().join("out"), limits)
    }

    #[test]
    fn a_good_site_unpacks_with_its_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("site.tar.gz");
        std::fs::write(
            &path,
            archive(&[
                Entry::Dir("./"),
                Entry::File("./index.html", b"<h1>hi</h1>"),
                Entry::Dir("./guide/"),
                Entry::File("./guide/style.css", b"body{}"),
                Entry::File("logo.png", b"\x89PNG"),
            ]),
        )
        .unwrap();
        let out = dir.path().join("out");
        let unpacked = unpack(&path, &out, LIMITS).unwrap();
        assert_eq!(unpacked.entries, 4);
        assert_eq!(unpacked.unpacked_size, 11 + 6 + 4);
        let index = &unpacked.files[0];
        assert_eq!(index.path, "index.html");
        assert_eq!(index.content_type, "text/html; charset=utf-8");
        assert_eq!(
            index.sha256,
            hex(&Sha256::digest(b"<h1>hi</h1>")),
            "the sha256 of the content"
        );
        assert_eq!(unpacked.files[1].path, "guide/style.css");
        assert_eq!(unpacked.files[1].content_type, "text/css; charset=utf-8");
        assert_eq!(unpacked.files[2].content_type, "image/png");
        assert_eq!(
            std::fs::read(out.join("guide/style.css")).unwrap(),
            b"body{}"
        );
    }

    fn refused(entries: &[Entry]) -> ArchiveError {
        run(&archive(entries), LIMITS).unwrap_err()
    }

    #[test]
    fn each_refusal_names_its_path() {
        let cases: Vec<(ArchiveError, &str)> = vec![
            (
                refused(&[
                    Entry::Symlink("link", "/etc/passwd"),
                    Entry::File("index.html", b"x"),
                ]),
                "\"link\"",
            ),
            (
                refused(&[
                    Entry::File("index.html", b"x"),
                    Entry::HardLink("hard", "index.html"),
                ]),
                "\"hard\"",
            ),
            (
                refused(&[Entry::File("index.html", b"x"), Entry::Fifo("pipe")]),
                "\"pipe\"",
            ),
            (
                refused(&[Entry::File("/etc/cron.d/x", b"x")]),
                "\"/etc/cron.d/x\"",
            ),
            (refused(&[Entry::File("a/../../x", b"x")]), "\"a/../../x\""),
            (
                refused(&[Entry::File("a\\b.html", b"x")]),
                "\"a\\\\b.html\"",
            ),
            (
                refused(&[
                    Entry::File("index.html", b"x"),
                    Entry::File("./index.html", b"y"),
                ]),
                "\"index.html\"",
            ),
            (
                refused(&[Entry::File("dist/index.html", b"x")]),
                "index.html at its root",
            ),
        ];
        for (error, named) in cases {
            assert!(
                error.to_string().contains(named),
                "{error} should name {named}"
            );
        }
    }

    #[test]
    fn the_refusals_have_their_kinds() {
        assert_eq!(
            refused(&[Entry::Symlink("link", "x")]),
            ArchiveError::NotAFile("link".into())
        );
        assert_eq!(
            refused(&[Entry::File("/abs", b"x")]),
            ArchiveError::Absolute("/abs".into())
        );
        assert_eq!(
            refused(&[Entry::File("../x", b"x")]),
            ArchiveError::DotDot("../x".into())
        );
        assert_eq!(
            refused(&[
                Entry::File("index.html", b"x"),
                Entry::File("index.html", b"x")
            ]),
            ArchiveError::Duplicate("index.html".into())
        );
        assert_eq!(
            refused(&[Entry::File("other.html", b"x")]),
            ArchiveError::NoIndex
        );
        let long = "a/".repeat(600) + "x";
        assert!(matches!(
            refused(&[Entry::File(&long, b"x")]),
            ArchiveError::PathTooLong { len: 1201, .. }
        ));
    }

    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        assert_eq!(site_path(b"a\xffb"), Err(ArchiveError::NotUtf8));
    }

    #[test]
    fn the_entry_limit_names_the_limit() {
        let limits = Limits {
            max_size: 1 << 20,
            max_files: 2,
        };
        let err = run(
            &archive(&[
                Entry::File("index.html", b"x"),
                Entry::File("a.html", b"x"),
                Entry::File("b.html", b"x"),
            ]),
            limits,
        )
        .unwrap_err();
        assert_eq!(err, ArchiveError::TooManyEntries { limit: 2 });
        assert!(err.to_string().contains("The limit is 2"), "{err}");
    }

    #[test]
    fn the_size_limit_names_the_limit_and_the_path() {
        let limits = Limits {
            max_size: 1000,
            max_files: 100,
        };
        let err = run(
            &archive(&[
                Entry::File("index.html", b"x"),
                Entry::Sized("big.bin", 1500),
            ]),
            limits,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ArchiveError::TooLarge {
                limit: 1000,
                path: "big.bin".into()
            }
        );
        assert!(err.to_string().contains("1000 bytes"), "{err}");
    }

    /// A compressed bomb: 64 MiB of zeros compress to some KiB. Unpacking
    /// stops at the limit, before it writes the file.
    #[test]
    fn a_compressed_bomb_stops_at_the_limit() {
        let bomb = archive(&[
            Entry::File("index.html", b"x"),
            Entry::Sized("zeros.bin", 64 << 20),
        ]);
        assert!(
            bomb.len() < 1 << 20,
            "the bomb is small: {} bytes",
            bomb.len()
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bomb.tar.gz");
        std::fs::write(&path, &bomb).unwrap();
        let out = dir.path().join("out");
        let started = std::time::Instant::now();
        let err = unpack(
            &path,
            &out,
            Limits {
                max_size: 1 << 20,
                max_files: 100,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ArchiveError::TooLarge { .. }), "{err}");
        assert!(
            !out.join("zeros.bin").exists(),
            "nothing of the bomb is written"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn not_a_tar_gz_is_refused() {
        let err = run(b"plain text, not gzip", LIMITS).unwrap_err();
        assert!(matches!(err, ArchiveError::NotATarGz(_)), "{err}");
    }
}
