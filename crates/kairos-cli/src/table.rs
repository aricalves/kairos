//! A tiny aligned-column table renderer for the human-readable output mode
//! (KAIROS-A-0015: "human-readable tables by default" — plain text, no TUI
//! dependency; `--json` is the scripting surface).

/// A simple text table: headers plus rows, rendered with columns padded to
/// their widest cell and separated by two spaces.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// A table with the given column headers.
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
        }
    }

    /// Append one row. Rows shorter than the header count render as blank
    /// trailing cells; longer rows keep their extra cells unaligned.
    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// Render with each column padded to its widest cell (headers
    /// included), two spaces between columns, no trailing whitespace.
    pub fn render(&self) -> String {
        let columns = self
            .rows
            .iter()
            .map(Vec::len)
            .chain([self.headers.len()])
            .max()
            .unwrap_or(0);
        let mut widths = vec![0usize; columns];
        for line in std::iter::once(&self.headers).chain(&self.rows) {
            for (i, cell) in line.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }

        let render_line = |line: &[String]| -> String {
            let mut out = String::new();
            for (i, cell) in line.iter().enumerate() {
                if i > 0 {
                    out.push_str("  ");
                }
                out.push_str(cell);
                // Pad every column but the last so lines never carry
                // trailing spaces.
                if i + 1 < line.len() {
                    let pad = widths[i].saturating_sub(cell.chars().count());
                    out.extend(std::iter::repeat_n(' ', pad));
                }
            }
            out
        };

        let mut out = String::new();
        out.push_str(&render_line(&self.headers));
        out.push('\n');
        for row in &self.rows {
            out.push_str(&render_line(row));
            out.push('\n');
        }
        out
    }
}

/// The footer of one page of a list (KAIROS-T-0593): the `total:` line,
/// then `Next: --offset N` when more rows follow the page. `shown` is the
/// number of rows on the page. Pure.
pub fn page_footer(total: i64, limit: i64, offset: i64, shown: usize) -> String {
    let mut out = format!("total: {total} (limit {limit}, offset {offset})\n");
    let shown = i64::try_from(shown).unwrap_or(i64::MAX);
    let next = offset.saturating_add(shown);
    if shown > 0 && next < total {
        out.push_str(&format!("Next: --offset {next}\n"));
    }
    out
}

/// Print [`page_footer`].
pub fn print_page_footer(total: i64, limit: i64, offset: i64, shown: usize) {
    print!("{}", page_footer(total, limit, offset, shown));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Columns align to the widest cell, headers included; no trailing
    /// whitespace on any line.
    #[test]
    fn renders_aligned_columns() {
        let mut table = Table::new(&["CODE", "TITLE", "VER"]);
        table.row(vec!["ACME-T-0001".into(), "Fix login".into(), "3".into()]);
        table.row(vec!["ACME-T-0002".into(), "A".into(), "12".into()]);
        let rendered = table.render();
        assert_eq!(
            rendered,
            "CODE         TITLE      VER\n\
             ACME-T-0001  Fix login  3\n\
             ACME-T-0002  A          12\n"
        );
        assert!(rendered.lines().all(|l| l.trim_end() == l), "{rendered}");
    }

    /// A header-only table renders just the header line.
    #[test]
    fn renders_empty_table() {
        let table = Table::new(&["A", "B"]);
        assert_eq!(table.render(), "A  B\n");
    }

    /// A page with rows after it names the next offset; the last page, an
    /// empty page and a page past the end do not (KAIROS-T-0593).
    #[test]
    fn the_footer_names_the_next_offset_when_more_rows_follow() {
        assert_eq!(
            page_footer(340, 200, 0, 200),
            "total: 340 (limit 200, offset 0)\nNext: --offset 200\n"
        );
        assert_eq!(
            page_footer(3, 1, 1, 1),
            "total: 3 (limit 1, offset 1)\nNext: --offset 2\n"
        );
        assert_eq!(
            page_footer(340, 200, 200, 140),
            "total: 340 (limit 200, offset 200)\n"
        );
        assert_eq!(page_footer(0, 50, 0, 0), "total: 0 (limit 50, offset 0)\n");
        assert_eq!(
            page_footer(3, 50, 10, 0),
            "total: 3 (limit 50, offset 10)\n"
        );
        // The server shrank the page below the limit: the next offset
        // follows the rows shown, not the limit.
        assert_eq!(
            page_footer(10, 200, 0, 4),
            "total: 10 (limit 200, offset 0)\nNext: --offset 4\n"
        );
    }
}
