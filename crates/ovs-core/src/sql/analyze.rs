use std::ops::Range;

use super::lexer::{TokenKind, tokenize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementKind {
    /// `SELECT`, `WITH`, `TABLE`, `VALUES` and parenthesised selects.
    Select,
    /// `SHOW`, `DESCRIBE`, `EXPLAIN` and other read-only introspection.
    Show,
    Insert,
    Update,
    Delete,
    /// `CREATE`, `ALTER`, `DROP`, `RENAME`, `TRUNCATE`.
    Ddl,
    /// `USE db`, with the unquoted database name.
    Use(String),
    Other,
}

impl StatementKind {
    /// Statements that may change which databases or objects exist, so the object browser
    /// should refresh afterwards.
    pub fn changes_schema(&self) -> bool {
        matches!(self, Self::Ddl)
    }
}

/// Words (upper-cased) at nesting depth 0, ignoring trivia.
fn top_level_words(sql: &str) -> Vec<String> {
    let mut depth = 0i32;
    let mut out = Vec::new();
    for tok in tokenize(sql) {
        match tok.kind {
            TokenKind::Punct if tok.text(sql) == "(" => depth += 1,
            TokenKind::Punct if tok.text(sql) == ")" => depth -= 1,
            TokenKind::Word if depth == 0 => out.push(tok.text(sql).to_ascii_uppercase()),
            _ => {}
        }
    }
    out
}

pub fn classify(sql: &str) -> StatementKind {
    let tokens = tokenize(sql);
    let mut code = tokens
        .iter()
        .filter(|t| !t.kind.is_trivia() && (t.kind != TokenKind::Punct || t.text(sql) != "("));
    let Some(first) = code.next() else {
        return StatementKind::Other;
    };
    match first.text(sql).to_ascii_uppercase().as_str() {
        "SELECT" | "WITH" | "TABLE" | "VALUES" => StatementKind::Select,
        "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "HELP" => StatementKind::Show,
        "INSERT" | "REPLACE" => StatementKind::Insert,
        "UPDATE" => StatementKind::Update,
        "DELETE" => StatementKind::Delete,
        "CREATE" | "ALTER" | "DROP" | "RENAME" | "TRUNCATE" => StatementKind::Ddl,
        "USE" => code.next().map_or(StatementKind::Other, |t| {
            StatementKind::Use(unquote(t.text(sql)))
        }),
        _ => StatementKind::Other,
    }
}

fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'`' | b'"') && bytes[bytes.len() - 1] == bytes[0] {
        let q = &s[..1];
        s[1..s.len() - 1].replace(&format!("{q}{q}"), q)
    } else {
        s.to_owned()
    }
}

/// Appends `LIMIT n` to a plain `SELECT` that has no limit of its own, so browsing a huge table
/// doesn't pull every row. Returns `None` when the statement should run unchanged.
pub fn apply_row_limit(sql: &str, limit: usize) -> Option<String> {
    if classify(sql) != StatementKind::Select {
        return None;
    }
    let words = top_level_words(sql);
    let blocked = [
        "LIMIT",
        "INTO",
        "FOR",
        "LOCK",
        "PROCEDURE",
        "FETCH",
        "OFFSET",
    ];
    if words.iter().any(|w| blocked.contains(&w.as_str())) {
        return None;
    }
    Some(format!("{} LIMIT {limit}", sql.trim_end()))
}

/// A top-level `LIMIT` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitClause {
    pub row_count: u64,
    pub offset: u64,
    /// Byte range of the clause, from `LIMIT` to its last number.
    pub range: Range<usize>,
}

/// Finds the statement's own top-level `LIMIT n`, `LIMIT offset, n` or `LIMIT n OFFSET m`.
/// Limits inside subqueries are ignored, and so is a `LIMIT` with placeholders or variables.
pub fn limit_clause(sql: &str) -> Option<LimitClause> {
    let tokens: Vec<_> = tokenize(sql)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let mut depth = 0i32;
    for (i, tok) in tokens.iter().enumerate() {
        match tok.kind {
            TokenKind::Punct if tok.text(sql) == "(" => depth += 1,
            TokenKind::Punct if tok.text(sql) == ")" => depth -= 1,
            TokenKind::Word if depth == 0 && tok.text(sql).eq_ignore_ascii_case("limit") => {
                let number = |j: usize| -> Option<u64> {
                    let t = tokens.get(j)?;
                    (t.kind == TokenKind::Number).then(|| t.text(sql).parse().ok())?
                };
                let first = number(i + 1)?;
                let next = tokens.get(i + 2);
                let (row_count, offset, last) = match next.map(|t| t.text(sql)) {
                    Some(",") => (number(i + 3)?, first, i + 3),
                    Some(w) if w.eq_ignore_ascii_case("offset") => (first, number(i + 3)?, i + 3),
                    _ => (first, 0, i + 1),
                };
                return Some(LimitClause {
                    row_count,
                    offset,
                    range: tok.range.start..tokens[last].range.end,
                });
            }
            _ => {}
        }
    }
    None
}

/// Wraps a `SELECT` so it returns the number of rows the original would produce without its
/// own top-level `LIMIT`. Used to show "Showing 50 of 20,000 rows".
pub fn count_query(sql: &str) -> String {
    let sql = sql.trim();
    let body = match limit_clause(sql) {
        Some(limit) => format!("{}{}", &sql[..limit.range.start], &sql[limit.range.end..]),
        None => sql.to_owned(),
    };
    format!("SELECT COUNT(*) FROM ({}) AS ovs_count", body.trim())
}

/// True for an `UPDATE` or `DELETE` with no top-level `WHERE`, i.e. one that would touch every
/// row of the table.
pub fn missing_where(sql: &str) -> bool {
    matches!(classify(sql), StatementKind::Update | StatementKind::Delete)
        && !top_level_words(sql).iter().any(|w| w == "WHERE")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_statements() {
        assert_eq!(classify("  -- hi\n select 1"), StatementKind::Select);
        assert_eq!(
            classify("(SELECT 1) UNION (SELECT 2)"),
            StatementKind::Select
        );
        assert_eq!(classify("show tables"), StatementKind::Show);
        assert_eq!(classify("drop table t"), StatementKind::Ddl);
        assert_eq!(classify("USE `my``db`"), StatementKind::Use("my`db".into()));
        assert_eq!(classify("use sales"), StatementKind::Use("sales".into()));
        assert_eq!(classify(""), StatementKind::Other);
    }

    #[test]
    fn row_limit_only_for_unlimited_selects() {
        assert_eq!(
            apply_row_limit("select * from t ", 1000).as_deref(),
            Some("select * from t LIMIT 1000")
        );
        assert_eq!(apply_row_limit("select * from t limit 5", 10), None);
        assert_eq!(apply_row_limit("select * from t for update", 10), None);
        assert_eq!(apply_row_limit("select 1 into @x", 10), None);
        assert_eq!(apply_row_limit("update t set a=1", 10), None);
        // A LIMIT inside a subquery doesn't count.
        assert!(apply_row_limit("select * from (select * from t limit 1) x", 10).is_some());
    }

    #[test]
    fn wraps_select_for_counting() {
        assert_eq!(
            count_query(" select * from t order by a "),
            "SELECT COUNT(*) FROM (select * from t order by a) AS ovs_count"
        );
        assert_eq!(
            count_query("select * from t limit 50"),
            "SELECT COUNT(*) FROM (select * from t) AS ovs_count"
        );
    }

    #[test]
    fn finds_top_level_limit() {
        let l = limit_clause("SELECT * FROM t LIMIT 50").unwrap();
        assert_eq!((l.row_count, l.offset), (50, 0));
        let l = limit_clause("select 1 from t limit 10, 25").unwrap();
        assert_eq!((l.row_count, l.offset), (25, 10));
        let l = limit_clause("select 1 from t limit 25 offset 10 for update").unwrap();
        assert_eq!((l.row_count, l.offset), (25, 10));
        assert_eq!(limit_clause("select * from (select 1 limit 5) x"), None);
        assert_eq!(limit_clause("select * from t limit ?"), None);
    }

    #[test]
    fn detects_missing_where() {
        assert!(missing_where("DELETE FROM t"));
        assert!(missing_where("update t set a = (select 1 where 1)"));
        assert!(!missing_where("update t set a=1 where id=2"));
        assert!(!missing_where("select * from t"));
    }
}
