//! The lists of the CLI (KAIROS-T-0644, KAIROS-A-0057): one page or each
//! page of a list, the `--json` envelope, and the footer of the text.
//!
//! Each `list` verb and `search` print one envelope with `--json`:
//!
//! ```json
//! { "items": [ ... ], "total": 340, "next_offset": 200 }
//! ```
//!
//! `next_offset` is the `--offset` of the next page, and `null` on the last
//! page. A list that the server does not page has `total` equal to the
//! number of rows and `next_offset` null. `--all` reads each page.

use std::future::Future;

use kairos_client::types::{ListEnvelope, Pagination};
use serde::Serialize;

use crate::context::print_json;
use crate::error::CliError;
use crate::table::page_footer;

/// The page size that `--all` asks for. The server clamps it to its
/// maximum: 200 for a list, 100 for a search.
pub const ALL_PAGE: i64 = 200;

/// The `--limit`, `--offset` and `--all` flags of a paged list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Paging {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub all: bool,
}

/// The `--offset` of the page after a page of `shown` rows at `offset`, or
/// `None` when no row follows. Pure.
pub fn next_offset(total: i64, offset: i64, shown: usize) -> Option<i64> {
    let shown = i64::try_from(shown).unwrap_or(i64::MAX);
    let next = offset.saturating_add(shown);
    (shown > 0 && next < total).then_some(next)
}

/// The `--json` form of a list: the contract for scripts (KAIROS-A-0057).
#[derive(Debug, Serialize)]
pub struct ListJson<'a, T> {
    pub items: &'a [T],
    pub total: i64,
    pub next_offset: Option<i64>,
}

/// Print a list that the server does not page: `total` is the number of
/// rows, `next_offset` is null.
pub fn print_whole<T: Serialize>(items: &[T]) -> Result<(), CliError> {
    print_json(&ListJson {
        items,
        total: i64::try_from(items.len()).unwrap_or(i64::MAX),
        next_offset: None,
    })
}

/// One page of a list, or each page (`--all`).
#[derive(Debug)]
pub struct Listing<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
    /// True when the listing holds each page.
    pub all: bool,
}

impl<T> Listing<T> {
    /// The listing of one server page.
    pub fn page(envelope: ListEnvelope<T>) -> Self
    where
        T: utoipa::ToSchema,
    {
        Self {
            items: envelope.items,
            total: envelope.total,
            limit: envelope.limit,
            offset: envelope.offset,
            all: false,
        }
    }

    /// The `--offset` of the next page; `None` with `--all` and on the
    /// last page.
    pub fn next_offset(&self) -> Option<i64> {
        if self.all {
            return None;
        }
        next_offset(self.total, self.offset, self.items.len())
    }

    /// The `--json` envelope.
    pub fn json(&self) -> ListJson<'_, T> {
        ListJson {
            items: &self.items,
            total: self.total,
            next_offset: self.next_offset(),
        }
    }

    /// Print the `--json` envelope.
    pub fn print_json(&self) -> Result<(), CliError>
    where
        T: Serialize,
    {
        print_json(&self.json())
    }

    /// The text after the table: `total: N (limit L, offset O)` and the
    /// `Next:` line of one page, or `total: N` for each page. Pure.
    pub fn footer(&self) -> String {
        if self.all {
            format!("total: {}\n", self.total)
        } else {
            page_footer(self.total, self.limit, self.offset, self.items.len())
        }
    }

    /// Print [`Listing::footer`].
    pub fn print_footer(&self) {
        print!("{}", self.footer());
    }
}

/// Read each page from offset 0 until no row follows: `page` gets the
/// offset and gives the rows and the total of the page. A page with no row
/// ends the read, so a server that loses rows cannot keep the loop open.
pub async fn read_each_page<T, F, Fut>(mut page: F) -> Result<(Vec<T>, i64), CliError>
where
    F: FnMut(i64) -> Fut,
    Fut: Future<Output = Result<(Vec<T>, i64), CliError>>,
{
    let mut items: Vec<T> = Vec::new();
    loop {
        let offset = i64::try_from(items.len()).unwrap_or(i64::MAX);
        let (rows, total) = page(offset).await?;
        let shown = rows.len();
        items.extend(rows);
        if next_offset(total, offset, shown).is_none() {
            return Ok((items, total));
        }
    }
}

/// One page of a paged list, or each page with `--all`.
pub async fn fetch<T, F, Fut>(paging: Paging, mut page: F) -> Result<Listing<T>, CliError>
where
    T: utoipa::ToSchema,
    F: FnMut(Pagination) -> Fut,
    Fut: Future<Output = Result<ListEnvelope<T>, kairos_client::Error>>,
{
    if !paging.all {
        let envelope = page(Pagination {
            limit: paging.limit,
            offset: paging.offset,
        })
        .await?;
        return Ok(Listing::page(envelope));
    }
    let (items, total) = read_each_page(|offset| {
        let next = page(Pagination {
            limit: Some(ALL_PAGE),
            offset: Some(offset),
        });
        async move {
            let envelope = next.await?;
            Ok::<_, CliError>((envelope.items, envelope.total))
        }
    })
    .await?;
    Ok(Listing {
        items,
        total,
        limit: ALL_PAGE,
        offset: 0,
        all: true,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn the_next_offset_follows_the_rows_shown() {
        assert_eq!(next_offset(340, 0, 200), Some(200));
        assert_eq!(next_offset(340, 200, 140), None);
        assert_eq!(next_offset(3, 1, 1), Some(2));
        assert_eq!(next_offset(0, 0, 0), None);
        // A page past the end, and an empty page: no next page.
        assert_eq!(next_offset(3, 10, 0), None);
        // The server shrank the page below the limit.
        assert_eq!(next_offset(10, 0, 4), Some(4));
    }

    /// The envelope has three keys, in this order, and `next_offset` is
    /// null on the last page (KAIROS-A-0057).
    #[test]
    fn the_envelope_has_items_total_and_next_offset() {
        let first = Listing {
            items: vec!["a", "b"],
            total: 3,
            limit: 2,
            offset: 0,
            all: false,
        };
        assert_eq!(
            serde_json::to_string(&first.json()).unwrap(),
            r#"{"items":["a","b"],"total":3,"next_offset":2}"#
        );
        let last = Listing {
            items: vec!["c"],
            total: 3,
            limit: 2,
            offset: 2,
            all: false,
        };
        assert_eq!(
            serde_json::to_string(&last.json()).unwrap(),
            r#"{"items":["c"],"total":3,"next_offset":null}"#
        );
    }

    #[test]
    fn the_footer_of_each_page_gives_the_total_only() {
        let all = Listing {
            items: vec![1, 2, 3],
            total: 3,
            limit: ALL_PAGE,
            offset: 0,
            all: true,
        };
        assert_eq!(all.footer(), "total: 3\n");
        assert_eq!(all.next_offset(), None);
        let page = Listing {
            items: vec![1],
            total: 3,
            limit: 1,
            offset: 0,
            all: false,
        };
        assert_eq!(
            page.footer(),
            "total: 3 (limit 1, offset 0)\nNext: --offset 1\n"
        );
    }

    /// A server page of `limit` rows from `rows` fixtures, with the offsets
    /// that the read asked for.
    async fn fixture_page(
        rows: usize,
        asked: &Mutex<Vec<(Option<i64>, Option<i64>)>>,
        page: Pagination,
    ) -> Result<ListEnvelope<String>, kairos_client::Error> {
        asked.lock().unwrap().push((page.limit, page.offset));
        let offset = page.offset.unwrap_or(0);
        // The server clamps the limit to 200.
        let limit = page.limit.unwrap_or(50).clamp(1, 200);
        let items = (0..rows)
            .skip(usize::try_from(offset).unwrap())
            .take(usize::try_from(limit).unwrap())
            .map(|n| format!("row-{n}"))
            .collect();
        Ok(ListEnvelope {
            items,
            total: i64::try_from(rows).unwrap(),
            limit,
            offset,
        })
    }

    /// `--all` reads each page of 200 until the last row, and keeps the
    /// order of the rows.
    #[tokio::test]
    async fn all_reads_each_page() {
        let asked = Mutex::new(Vec::new());
        let paging = Paging {
            all: true,
            ..Paging::default()
        };
        let listing = fetch(paging, |page| fixture_page(450, &asked, page))
            .await
            .unwrap();
        assert_eq!(listing.items.len(), 450);
        assert_eq!(listing.items[0], "row-0");
        assert_eq!(listing.items[449], "row-449");
        assert_eq!(listing.total, 450);
        assert_eq!(listing.next_offset(), None);
        assert_eq!(
            *asked.lock().unwrap(),
            vec![
                (Some(200), Some(0)),
                (Some(200), Some(200)),
                (Some(200), Some(400))
            ]
        );

        // An empty list is one call.
        asked.lock().unwrap().clear();
        let empty = fetch(paging, |page| fixture_page(0, &asked, page))
            .await
            .unwrap();
        assert!(empty.items.is_empty());
        assert_eq!(asked.lock().unwrap().len(), 1);
    }

    /// Without `--all`, the CLI asks for the page of the flags, once.
    #[tokio::test]
    async fn one_page_without_all() {
        let asked = Mutex::new(Vec::new());
        let paging = Paging {
            limit: Some(10),
            offset: Some(20),
            all: false,
        };
        let listing = fetch(paging, |page| fixture_page(450, &asked, page))
            .await
            .unwrap();
        assert_eq!(listing.items.len(), 10);
        assert_eq!(listing.next_offset(), Some(30));
        assert_eq!(*asked.lock().unwrap(), vec![(Some(10), Some(20))]);
    }

    /// A server that gives fewer rows than its total ends the read at the
    /// first empty page.
    #[tokio::test]
    async fn an_empty_page_ends_the_read() {
        let calls = Mutex::new(0);
        let (rows, total) = read_each_page(|offset| {
            *calls.lock().unwrap() += 1;
            async move {
                let rows: Vec<i64> = if offset == 0 { vec![1, 2] } else { vec![] };
                Ok::<_, CliError>((rows, 10))
            }
        })
        .await
        .unwrap();
        assert_eq!(rows, vec![1, 2]);
        assert_eq!(total, 10);
        assert_eq!(*calls.lock().unwrap(), 2);
    }
}
