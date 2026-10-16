//! The notes of an item (KAIROS-T-0504): the `## Note …` headings that
//! `append_note` adds, each one with an id.
//!
//! A heading is `## Note n-3 (2026-10-15, Robin)`. A note from before the
//! ids has the heading `## Note (2026-10-15, Robin)`; it gets its id from
//! its position when read (n-1, n-2, …), so the numbering is stable. A new
//! note gets the id after the count of the note headings of the content.
//!
//! The parser is here, not in kairos-core, so that the web app (wasm32)
//! and the server share one reading of a heading.

/// The id of the note at `position` (one-based): `n-3`.
pub fn note_id(position: usize) -> String {
    format!("n-{position}")
}

/// The heading of a note: `## Note n-3 (2026-10-15, Robin)`.
pub fn note_heading(position: usize, date: &str, author: &str) -> String {
    format!("## Note {} ({date}, {})", note_id(position), author.trim())
}

/// The parts of the title of a note heading (the text after `## `).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteTitle<'a> {
    /// The id in the heading, when it has one.
    pub id: Option<&'a str>,
    /// The date, as written.
    pub date: &'a str,
    /// The author, as written.
    pub author: &'a str,
}

/// Parse the title of a heading: `Note (2026-10-15, Robin)` or
/// `Note n-3 (2026-10-15, Robin)`. `None` for any other heading.
pub fn parse_note_title(title: &str) -> Option<NoteTitle<'_>> {
    let rest = title.trim().strip_prefix("Note")?;
    let rest = rest.strip_suffix(')')?;
    let (head, inside) = rest.split_once('(')?;
    let id = match head.trim() {
        "" => None,
        id if is_note_id(id) => Some(id),
        _ => return None,
    };
    let (date, author) = inside.split_once(',')?;
    let (date, author) = (date.trim(), author.trim());
    if date.is_empty() || author.is_empty() {
        return None;
    }
    Some(NoteTitle { id, date, author })
}

/// Is `id` of the form `n-<digits>`?
pub fn is_note_id(id: &str) -> bool {
    id.strip_prefix("n-")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// One note of an item, as read from the content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// `n-3`: from the heading, or from the position of the note.
    pub id: String,
    pub date: String,
    pub author: String,
    /// The first line of the text of the note with words; empty when the
    /// note has none.
    pub first_line: String,
}

/// The notes of `content`, in order. A heading inside a fenced code block
/// is not a note.
pub fn notes(content: &str) -> Vec<Note> {
    let mut found: Vec<Note> = Vec::new();
    // The note whose first line is still wanted.
    let mut open: Option<usize> = None;
    let mut in_fence = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(title) = trimmed.strip_prefix("## ") {
            // Any `## ` heading ends the text of the open note.
            open = None;
            if let Some(parsed) = parse_note_title(title) {
                let position = found.len() + 1;
                found.push(Note {
                    id: parsed.id.map_or_else(|| note_id(position), str::to_string),
                    date: parsed.date.to_string(),
                    author: parsed.author.to_string(),
                    first_line: String::new(),
                });
                open = Some(position - 1);
            }
            continue;
        }
        if let Some(index) = open
            && !trimmed.is_empty()
        {
            found[index].first_line = trimmed.to_string();
            open = None;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_heading_carries_the_id() {
        assert_eq!(
            note_heading(3, "2026-10-15", " Robin "),
            "## Note n-3 (2026-10-15, Robin)"
        );
    }

    #[test]
    fn a_title_with_and_without_an_id_parses() {
        assert_eq!(
            parse_note_title("Note (2026-10-11, Robin)"),
            Some(NoteTitle {
                id: None,
                date: "2026-10-11",
                author: "Robin"
            })
        );
        assert_eq!(
            parse_note_title("Note n-12 (2026-10-11, Sam Lee)"),
            Some(NoteTitle {
                id: Some("n-12"),
                date: "2026-10-11",
                author: "Sam Lee"
            })
        );
        for other in [
            "Notes",
            "Note",
            "Note (2026-10-11)",
            "Note x-1 (2026-10-11, Robin)",
            "Note n- (2026-10-11, Robin)",
            "Note (, Robin)",
            "Objective",
        ] {
            assert_eq!(parse_note_title(other), None, "{other}");
        }
    }

    #[test]
    fn old_notes_get_an_id_by_position_and_new_ones_keep_theirs() {
        let content = "# Pay\n\n## Objective\n\nx\n\n## Note (2026-10-01, Robin)\n\nFirst.\nMore.\n\n## Note (2026-10-02, Sam)\n\n## Next\n\nnot a note\n\n```\n## Note (2026-10-03, Fake)\n```\n\n## Note n-3 (2026-10-15, Robin)\n\n- Third, in a list.\n";
        let found = notes(content);
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(found[0].id, "n-1");
        assert_eq!(found[0].author, "Robin");
        assert_eq!(found[0].first_line, "First.");
        assert_eq!(found[1].id, "n-2");
        assert_eq!(found[1].first_line, "");
        assert_eq!(found[2].id, "n-3");
        assert_eq!(found[2].date, "2026-10-15");
        assert_eq!(found[2].first_line, "- Third, in a list.");
    }
}
