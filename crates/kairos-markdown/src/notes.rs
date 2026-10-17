//! The notes of an item (KAIROS-T-0504): the `## Note …` headings that
//! `append_note` adds, each one with an id.
//!
//! A heading is `## Note n-3 (2026-10-15, Robin)`. A note from before the
//! ids has the heading `## Note (2026-10-15, Robin)`; it gets its id from
//! its position when read (n-1, n-2, …), so the numbering is stable. A new
//! note gets the id after the highest id of the content (KAIROS-T-0551), so
//! a hand edit that removes a note does not give its id again.
//!
//! A `## ` heading ends a note. So `append_note` demotes each heading of
//! the text by one level ([`demote_headings`]) before it writes the note.
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

/// The number of a note id: 3 for `n-3`. `None` for a text that is not an
/// id.
pub fn note_number(id: &str) -> Option<usize> {
    if !is_note_id(id) {
        return None;
    }
    id[2..].parse().ok()
}

/// The position of the next note of `content` (KAIROS-T-0551): one after
/// the highest id of its notes. A hand edit can remove n-2 from n-1, n-2,
/// n-3; the next note is then n-4, not a second n-3.
pub fn next_position(content: &str) -> usize {
    notes(content)
        .iter()
        .filter_map(|note| note_number(&note.id))
        .max()
        .unwrap_or(0)
        + 1
}

/// The text with each heading of level 2 to 5 one level lower, and the
/// count of the headings that changed (KAIROS-T-0551). A `## ` heading
/// ends a note, so a note must not have one. A deeper heading also goes one
/// level lower, so the headings of the text keep their order. A level-6
/// heading stays: Markdown has no level 7, and it does not end a note. A
/// heading in a fenced code block stays. The other lines stay as they are.
pub fn demote_headings(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len() + 8);
    let mut demoted = 0;
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        let start = line.len() - line.trim_start().len();
        let rest = &line[start..];
        let level = rest.bytes().take_while(|&b| b == b'#').count();
        let after = &rest[level..];
        let is_heading = after.is_empty() || after.starts_with([' ', '\t', '\r', '\n']);
        if !in_fence && (2..=5).contains(&level) && is_heading {
            out.push_str(&line[..start]);
            out.push('#');
            out.push_str(rest);
            demoted += 1;
        } else {
            out.push_str(line);
        }
    }
    (out, demoted)
}

/// The code in the line `Promoted to ACME-A-0029.` of a note text
/// (KAIROS-T-0505, KAIROS-T-0551): the item that the note became. `None`
/// when the text has no such line. A line in a fenced code block does not
/// count.
pub fn promoted_to(text: &str) -> Option<&str> {
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let Some(code) = trimmed
            .strip_prefix("Promoted to ")
            .and_then(|rest| rest.strip_suffix('.'))
        else {
            continue;
        };
        if !code.is_empty()
            && code.contains('-')
            && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Some(code);
        }
    }
    None
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
    /// The text of the note: the lines after the heading up to the next
    /// `## ` heading, with no blank lines at the start or the end.
    pub text: String,
    /// The index of the heading line in the lines of the content.
    pub heading_line: usize,
    /// The index of the last line of the note with words, or of the
    /// heading when the note has none (KAIROS-T-0505: a line goes after
    /// it).
    pub last_line: usize,
}

/// The notes of `content`, in order. A heading inside a fenced code block
/// is not a note.
pub fn notes(content: &str) -> Vec<Note> {
    let mut found: Vec<Note> = Vec::new();
    // The note whose text is still open, and its lines.
    let mut open: Option<(usize, Vec<&str>)> = None;
    let mut in_fence = false;
    let close = |found: &mut Vec<Note>, open: Option<(usize, Vec<&str>)>| {
        if let Some((index, lines)) = open {
            found[index].text = lines.join("\n").trim().to_string();
        }
    };
    for (number, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            if let Some((_, lines)) = &mut open {
                lines.push(line);
            }
            continue;
        }
        if !in_fence && let Some(title) = trimmed.strip_prefix("## ") {
            // Any `## ` heading ends the text of the open note.
            close(&mut found, open.take());
            if let Some(parsed) = parse_note_title(title) {
                let position = found.len() + 1;
                found.push(Note {
                    id: parsed.id.map_or_else(|| note_id(position), str::to_string),
                    date: parsed.date.to_string(),
                    author: parsed.author.to_string(),
                    first_line: String::new(),
                    text: String::new(),
                    heading_line: number,
                    last_line: number,
                });
                open = Some((position - 1, Vec::new()));
            }
            continue;
        }
        if let Some((index, lines)) = &mut open {
            lines.push(line);
            if !trimmed.is_empty() {
                found[*index].last_line = number;
                if found[*index].first_line.is_empty() && !in_fence {
                    found[*index].first_line = trimmed.to_string();
                }
            }
        }
    }
    close(&mut found, open.take());
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
        assert_eq!(found[0].text, "First.\nMore.");
        assert_eq!((found[0].heading_line, found[0].last_line), (6, 9));
        assert_eq!(found[1].id, "n-2");
        assert_eq!(found[1].first_line, "");
        assert_eq!(found[1].text, "");
        assert_eq!((found[1].heading_line, found[1].last_line), (11, 11));
        assert_eq!(found[2].id, "n-3");
        assert_eq!(found[2].date, "2026-10-15");
        assert_eq!(found[2].first_line, "- Third, in a list.");
        assert_eq!(found[2].text, "- Third, in a list.");
        assert_eq!(found[2].last_line, 23);
    }

    // KAIROS-T-0505: the text of a note keeps its code blocks.
    #[test]
    fn the_text_of_a_note_keeps_a_code_block() {
        let content =
            "## Note n-1 (2026-10-15, Robin)\n\nSee:\n\n```\n## Not a heading\n```\n\nDone.\n";
        let found = notes(content);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "See:\n\n```\n## Not a heading\n```\n\nDone.");
        assert_eq!(found[0].first_line, "See:");
        assert_eq!(found[0].last_line, 8);
    }

    // KAIROS-T-0551: the next id is one after the highest id.
    #[test]
    fn the_next_position_is_one_after_the_highest_id() {
        assert_eq!(next_position("# T\n\nBody\n"), 1);
        let gap =
            "## Note n-1 (2026-10-01, Robin)\n\nOne.\n\n## Note n-3 (2026-10-03, Sam)\n\nThree.\n";
        assert_eq!(notes(gap).len(), 2);
        assert_eq!(next_position(gap), 4);
        let unordered = "## Note n-7 (2026-10-01, Robin)\n\n## Note n-2 (2026-10-02, Sam)\n";
        assert_eq!(next_position(unordered), 8);
        // A note from before the ids counts by its position.
        let old = "## Note (2026-10-01, Robin)\n\n## Note (2026-10-02, Sam)\n";
        assert_eq!(next_position(old), 3);
        assert_eq!(note_number("n-12"), Some(12));
        assert_eq!(note_number("x-1"), None);
    }

    // KAIROS-T-0551: a heading of the text goes one level lower.
    #[test]
    fn the_headings_of_a_text_go_one_level_lower() {
        let text = "Intro.\n\n## Findings\n\n  ### Detail\n#### Four\n##### Five\n###### Six\n# Top\n##\n##no space\n\n```\n## In a fence\n```\nEnd.";
        let (demoted, count) = demote_headings(text);
        assert_eq!(
            demoted,
            "Intro.\n\n### Findings\n\n  #### Detail\n##### Four\n###### Five\n###### Six\n# Top\n###\n##no space\n\n```\n## In a fence\n```\nEnd."
        );
        assert_eq!(count, 5);
        assert_eq!(
            demote_headings("No heading.\r\n"),
            ("No heading.\r\n".to_string(), 0)
        );
        // After the demotion, the note keeps all its text.
        let content = format!("## Note n-1 (2026-10-01, Robin)\n\n{demoted}\n");
        let found = notes(&content);
        assert_eq!(found.len(), 1);
        assert!(found[0].text.ends_with("End."), "{:?}", found[0].text);
    }

    // KAIROS-T-0551: the line that a promote adds names the new item.
    #[test]
    fn the_promoted_line_names_the_new_item() {
        assert_eq!(
            promoted_to("One.\n\nPromoted to ACME-A-0029."),
            Some("ACME-A-0029")
        );
        assert_eq!(
            promoted_to("  Promoted to ACME-D-0001.  \nMore."),
            Some("ACME-D-0001")
        );
        for text in [
            "One.",
            "Promoted to ACME-A-0029",
            "Promoted to the team.",
            "We promoted to ACME-A-0029.",
            "```\nPromoted to ACME-A-0029.\n```",
        ] {
            assert_eq!(promoted_to(text), None, "{text}");
        }
    }
}
