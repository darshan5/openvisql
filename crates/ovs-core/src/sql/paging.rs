//! Building the queries behind result paging, filtering and server-side sorting.
//!
//! The user's statement (without its own `LIMIT`) is the base. A plain page just appends
//! `LIMIT offset, count`. A filter or sort wraps the base as a derived table, so any `SELECT`,
//! including joins and `UNION`s, can be filtered by its output columns:
//!
//! ```sql
//! SELECT * FROM (<base>) AS ovs_q WHERE <filter> ORDER BY `col` DESC LIMIT 50, 50
//! ```

use super::dialect::Dialect;

/// A server-side sort on one result column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortSpec {
    pub column: String,
    pub descending: bool,
}

/// What to fetch: a window of the base statement's rows, optionally filtered and sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSpec {
    /// The statement without its own top-level `LIMIT`.
    pub base: String,
    /// A SQL condition on the result's columns; blank for none.
    pub filter: String,
    pub sort: Option<SortSpec>,
    pub offset: u64,
    pub page_size: u64,
}

impl PageSpec {
    fn filter(&self) -> Option<&str> {
        Some(self.filter.trim()).filter(|f| !f.is_empty())
    }

    /// The base with the filter applied, unpaged and unsorted.
    fn filtered(&self) -> String {
        match self.filter() {
            Some(filter) => format!("SELECT * FROM ({}) AS ovs_q WHERE {filter}", self.base),
            None => self.base.clone(),
        }
    }

    /// The query for the current page.
    pub fn page_query(&self, dialect: Dialect) -> String {
        let limit = format!("LIMIT {}, {}", self.offset, self.page_size);
        match (&self.sort, self.filter()) {
            (None, None) => format!("{} {limit}", self.base),
            (sort, _) => {
                let mut sql = if self.filter().is_some() {
                    self.filtered()
                } else {
                    format!("SELECT * FROM ({}) AS ovs_q", self.base)
                };
                if let Some(sort) = sort {
                    let dir = if sort.descending { "DESC" } else { "ASC" };
                    sql = format!("{sql} ORDER BY {} {dir}", dialect.quote_ident(&sort.column));
                }
                format!("{sql} {limit}")
            }
        }
    }

    /// Counts every row the filtered base returns.
    pub fn count_query(&self) -> String {
        format!("SELECT COUNT(*) FROM ({}) AS ovs_count", self.filtered())
    }

    pub fn has_previous(&self) -> bool {
        self.offset > 0
    }

    /// Whether a next page exists, given how many rows this page returned and the total
    /// (when it is known).
    pub fn has_next(&self, rows_on_page: u64, total: Option<u64>) -> bool {
        match total {
            Some(total) => self.offset + rows_on_page < total,
            // Unknown total: a full page suggests there's more.
            None => rows_on_page >= self.page_size,
        }
    }

    pub fn next(&mut self) {
        self.offset += self.page_size;
    }

    pub fn previous(&mut self) {
        self.offset = self.offset.saturating_sub(self.page_size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> PageSpec {
        PageSpec {
            base: "SELECT * FROM shop.orders".into(),
            filter: String::new(),
            sort: None,
            offset: 0,
            page_size: 50,
        }
    }

    #[test]
    fn plain_pages_append_a_limit() {
        let mut s = spec();
        assert_eq!(
            s.page_query(Dialect::MySql),
            "SELECT * FROM shop.orders LIMIT 0, 50"
        );
        s.next();
        assert_eq!(
            s.page_query(Dialect::MySql),
            "SELECT * FROM shop.orders LIMIT 50, 50"
        );
        assert_eq!(
            s.count_query(),
            "SELECT COUNT(*) FROM (SELECT * FROM shop.orders) AS ovs_count"
        );
        s.previous();
        s.previous();
        assert_eq!(s.offset, 0);
    }

    #[test]
    fn filters_and_sorts_wrap_the_base() {
        let s = PageSpec {
            filter: " status = 'paid' ".into(),
            sort: Some(SortSpec {
                column: "ordered at".into(),
                descending: true,
            }),
            offset: 100,
            ..spec()
        };
        assert_eq!(
            s.page_query(Dialect::MySql),
            "SELECT * FROM (SELECT * FROM shop.orders) AS ovs_q WHERE status = 'paid' \
             ORDER BY `ordered at` DESC LIMIT 100, 50"
        );
        assert_eq!(
            s.count_query(),
            "SELECT COUNT(*) FROM (SELECT * FROM (SELECT * FROM shop.orders) AS ovs_q \
             WHERE status = 'paid') AS ovs_count"
        );
        let sorted_only = PageSpec {
            sort: Some(SortSpec {
                column: "id".into(),
                descending: false,
            }),
            ..spec()
        };
        assert_eq!(
            sorted_only.page_query(Dialect::MySql),
            "SELECT * FROM (SELECT * FROM shop.orders) AS ovs_q ORDER BY `id` ASC LIMIT 0, 50"
        );
    }

    #[test]
    fn next_page_detection() {
        let mut s = spec();
        assert!(!s.has_previous());
        assert!(s.has_next(50, None));
        assert!(!s.has_next(20, None));
        assert!(s.has_next(50, Some(120)));
        s.offset = 100;
        assert!(s.has_previous());
        assert!(!s.has_next(20, Some(120)));
    }
}
