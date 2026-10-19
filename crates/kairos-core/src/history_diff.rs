//! The diff form of `item_history` (KAIROS-T-0651, amends KAIROS-A-0004).
//!
//! A version of an item is a full row or a diff row. A full row holds the
//! text. A diff row holds a line diff from one full row of the same item:
//! the newest full row before it. So a reader needs at most two rows for a
//! version, and a missing row between them does not break the text.
//!
//! [`plan_write`] chooses the form of a new version. A new version is a full
//! row when the item has no full row, when the newest full row is
//! [`SNAPSHOT_EVERY`] or more versions older, or when the diff is not
//! smaller than the text. Else it is a diff row.
//!
//! # The stored form
//!
//! A diff is a JSON list of steps, applied in order to the base text:
//!
//! - a number `n > 0`: copy the next `n` bytes of the base;
//! - a number `n < 0`: skip the next `-n` bytes of the base;
//! - a string: insert it.
//!
//! The steps must use the whole base. [`apply`] refuses a diff that does
//! not fit its base, so a wrong base gives an error and not a wrong text.
//!
//! This module is pure: no I/O (KAIROS-A-0009).

use std::time::Duration;

use serde_json::Value;
use similar::{ChangeTag, TextDiff};

/// A version is a full row at most this many versions after the newest
/// full row of its item. So versions 1, 11, 21, … of a new item are full.
pub const SNAPSHOT_EVERY: i32 = 10;

/// The longest time that the line diff of one write can take. After it,
/// the diff is still correct but can be larger.
const DIFF_TIMEOUT: Duration = Duration::from_millis(500);

/// The form of a new version, from [`plan_write`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredForm {
    /// Store the full text.
    Full,
    /// Store `diff`, a diff from the full row of `base_version`.
    Diff {
        /// The version of the full row that the diff applies to.
        base_version: i32,
        /// The diff, in the form of the module docs.
        diff: String,
    },
}

/// A diff that does not fit its base text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("The diff does not fit its base text: {0}.")]
pub struct DiffError(&'static str);

/// Choose the form of version `version` with the text `content`. `base` is
/// the newest full row of the item before `version`, as `(version, text)`,
/// or `None` when the item has no full row.
pub fn plan_write(version: i32, base: Option<(i32, &str)>, content: &str) -> StoredForm {
    let Some((base_version, base_text)) = base else {
        return StoredForm::Full;
    };
    if base_version >= version || version - base_version >= SNAPSHOT_EVERY {
        return StoredForm::Full;
    }
    let diff = encode(base_text, content);
    if diff.len() >= content.len() {
        return StoredForm::Full;
    }
    StoredForm::Diff { base_version, diff }
}

/// The diff from `base` to `new`, in the form of the module docs.
pub fn encode(base: &str, new: &str) -> String {
    let text_diff = TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(base, new);
    let mut steps: Vec<Value> = Vec::new();
    // The open step: a copy, a skip, or an insert.
    let mut copy = 0usize;
    let mut skip = 0usize;
    let mut insert = String::new();
    let flush =
        |steps: &mut Vec<Value>, copy: &mut usize, skip: &mut usize, insert: &mut String| {
            if *copy > 0 {
                steps.push(Value::from(*copy as i64));
                *copy = 0;
            }
            if *skip > 0 {
                steps.push(Value::from(-(*skip as i64)));
                *skip = 0;
            }
            if !insert.is_empty() {
                steps.push(Value::from(std::mem::take(insert)));
            }
        };
    for change in text_diff.iter_all_changes() {
        let value = change.value();
        match change.tag() {
            ChangeTag::Equal => {
                if skip > 0 || !insert.is_empty() {
                    flush(&mut steps, &mut copy, &mut skip, &mut insert);
                }
                copy += value.len();
            }
            ChangeTag::Delete => {
                if copy > 0 || !insert.is_empty() {
                    flush(&mut steps, &mut copy, &mut skip, &mut insert);
                }
                skip += value.len();
            }
            ChangeTag::Insert => {
                if copy > 0 {
                    flush(&mut steps, &mut copy, &mut skip, &mut insert);
                }
                insert.push_str(value);
            }
        }
    }
    flush(&mut steps, &mut copy, &mut skip, &mut insert);
    Value::Array(steps).to_string()
}

/// The text that `diff` gives from `base`. An error when the diff is not
/// in the form of the module docs, or when it does not use the whole base.
pub fn apply(base: &str, diff: &str) -> Result<String, DiffError> {
    let steps: Vec<Value> =
        serde_json::from_str(diff).map_err(|_| DiffError("it is not a JSON list"))?;
    let mut out = String::with_capacity(base.len());
    let mut at = 0usize;
    for step in &steps {
        match step {
            Value::Number(n) => {
                let n = n
                    .as_i64()
                    .ok_or(DiffError("a step is not a whole number"))?;
                if n == 0 {
                    return Err(DiffError("a step is zero"));
                }
                let len = usize::try_from(n.unsigned_abs())
                    .map_err(|_| DiffError("a step is too large"))?;
                let end = at
                    .checked_add(len)
                    .filter(|end| *end <= base.len())
                    .ok_or(DiffError("a step goes past the end of the base"))?;
                let part = base
                    .get(at..end)
                    .ok_or(DiffError("a step ends inside a character"))?;
                if n > 0 {
                    out.push_str(part);
                }
                at = end;
            }
            Value::String(text) => out.push_str(text),
            _ => return Err(DiffError("a step is not a number or a string")),
        }
    }
    if at != base.len() {
        return Err(DiffError("the steps do not use the whole base"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(base: &str, new: &str) {
        let diff = encode(base, new);
        assert_eq!(apply(base, &diff).as_deref(), Ok(new), "diff {diff}");
    }

    #[test]
    fn a_diff_gives_the_new_text_back() {
        round_trip("", "");
        round_trip("", "one\ntwo\n");
        round_trip("one\ntwo\n", "");
        round_trip("one\ntwo\nthree\n", "one\n2\nthree\n");
        round_trip("one\ntwo", "one\ntwo\nthree");
        round_trip("no newline", "no newline\n");
        round_trip("a\r\nb\r\n", "a\r\nc\r\n");
        round_trip("héllo\nwörld\n", "héllo\nwörld — ✓\n");
        round_trip("# T\n\nx\n\n## A\n\ny\n", "# T\n\n## A\n\ny\nz\n\n## B\n");
    }

    #[test]
    fn a_small_edit_of_a_large_text_gives_a_small_diff() {
        let base: String = (0..400)
            .map(|i| format!("line {i} of the text\n"))
            .collect();
        let new = base.replace("line 200 of", "line two hundred of");
        let diff = encode(&base, &new);
        assert!(diff.len() < 100, "{} bytes: {diff}", diff.len());
        assert_eq!(apply(&base, &diff).unwrap(), new);
    }

    #[test]
    fn a_diff_on_the_wrong_base_is_an_error() {
        let diff = encode("one\ntwo\n", "one\nthree\n");
        assert!(apply("one\ntwo\nmore\n", &diff).is_err());
        assert!(apply("one\n", &diff).is_err());
        assert!(apply("x", "[2]").is_err());
        assert!(apply("é", "[1,-1]").is_err(), "a step inside a character");
        assert!(apply("x", "[0,1]").is_err());
        assert!(apply("x", "{}").is_err());
        assert!(apply("x", "[1,true]").is_err());
        assert!(apply("x", "not json").is_err());
    }

    #[test]
    fn the_first_version_and_each_tenth_version_are_full() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix\n";
        let edit = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
        assert_eq!(plan_write(1, None, text), StoredForm::Full);
        assert!(matches!(
            plan_write(2, Some((1, text)), edit),
            StoredForm::Diff {
                base_version: 1,
                ..
            }
        ));
        assert!(matches!(
            plan_write(10, Some((1, text)), edit),
            StoredForm::Diff {
                base_version: 1,
                ..
            }
        ));
        assert_eq!(plan_write(11, Some((1, text)), edit), StoredForm::Full);
        // An old item with full rows only: the next version is a diff.
        assert!(matches!(
            plan_write(31, Some((30, text)), edit),
            StoredForm::Diff {
                base_version: 30,
                ..
            }
        ));
        // A base that is not older than the version: full.
        assert_eq!(plan_write(5, Some((5, text)), edit), StoredForm::Full);
    }

    #[test]
    fn a_diff_that_is_not_smaller_than_the_text_is_stored_full() {
        assert_eq!(plan_write(2, Some((1, "a\n")), "b\n"), StoredForm::Full);
        assert_eq!(plan_write(2, Some((1, "abc\n")), ""), StoredForm::Full);
    }
}
