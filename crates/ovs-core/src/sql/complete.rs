//! Autocomplete: works out what fits at the cursor and suggests it from a schema catalog.
//!
//! [`analyze`] reads the statement around the cursor: the word being typed, an optional
//! `qualifier.` before it, what the clause expects (a table after `FROM`, a database after
//! `USE`, otherwise columns and expressions), and the tables the statement references with
//! their aliases. [`Catalog::missing`] lists the schema data that analysis needs, the caller
//! loads it, and [`suggest`] builds the list.

use std::collections::{BTreeMap, BTreeSet};

use super::keywords::{FUNCTIONS, KEYWORDS, is_keyword};
use super::lexer::{Token, TokenKind, tokenize};
use super::splitter::split_statements;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SuggestionKind {
    Column,
    Table,
    View,
    Database,
    Function,
    Keyword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    pub kind: SuggestionKind,
    /// Extra information shown beside the label, e.g. a column's type and table.
    pub detail: String,
    /// Text inserted when picked; identifiers that need it are backtick-quoted.
    pub insert: String,
}

/// What the clause at the cursor expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// After `FROM`, `JOIN`, `UPDATE`, `INTO`, `TABLE`, `DESCRIBE`.
    Table,
    /// After `USE` or `DATABASE`.
    Database,
    /// Anywhere else: columns, functions and keywords.
    Expression,
}

/// A table named in the statement, with its alias if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub database: Option<String>,
    pub table: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// The partial word before the cursor.
    pub prefix: String,
    /// The identifier before a `.` directly preceding the prefix, unquoted.
    pub qualifier: Option<String>,
    pub expect: Expect,
    pub tables: Vec<TableRef>,
}

/// Schema data known so far, filled lazily by the caller.
#[derive(Debug, Default, Clone)]
pub struct Catalog {
    pub databases: Option<Vec<String>>,
    /// Database → `(name, is_view)` of its tables and views.
    pub objects: BTreeMap<String, Vec<(String, bool)>>,
    /// (database, table) → (column, type).
    pub columns: BTreeMap<(String, String), Vec<(String, String)>>,
}

/// Schema data [`suggest`] needs that the catalog doesn't have yet.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Missing {
    pub databases: bool,
    pub objects: Vec<String>,
    pub columns: Vec<(String, String)>,
}

impl Missing {
    pub fn is_empty(&self) -> bool {
        !self.databases && self.objects.is_empty() && self.columns.is_empty()
    }
}

fn unquote(ident: &str) -> String {
    let b = ident.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'`' | b'"') && b[b.len() - 1] == b[0] {
        let q = &ident[..1];
        ident[1..ident.len() - 1].replace(&format!("{q}{q}"), q)
    } else {
        ident.to_owned()
    }
}

fn is_ident(tok: &Token, src: &str) -> bool {
    match tok.kind {
        TokenKind::QuotedIdent => true,
        TokenKind::Word => !is_keyword(tok.text(src)),
        _ => false,
    }
}

fn upper(tok: &Token, src: &str) -> String {
    tok.text(src).to_ascii_uppercase()
}

/// Keywords after which a table name is expected.
const TABLE_KEYWORDS: [&str; 7] = [
    "FROM", "JOIN", "UPDATE", "INTO", "TABLE", "DESCRIBE", "DESC",
];

/// Reads the table references (`[db.]table [[AS] alias]`, comma-separated after `FROM`).
fn table_refs(src: &str, tokens: &[Token]) -> Vec<TableRef> {
    let mut refs = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let starts_ref = tokens[i].kind == TokenKind::Word
            && TABLE_KEYWORDS.contains(&upper(&tokens[i], src).as_str());
        i += 1;
        if !starts_ref {
            continue;
        }
        while let Some(first) = tokens.get(i).filter(|t| is_ident(t, src)) {
            let mut table = unquote(first.text(src));
            let mut database = None;
            i += 1;
            if tokens.get(i).is_some_and(|t| t.text(src) == ".")
                && tokens.get(i + 1).is_some_and(|t| is_ident(t, src))
            {
                database = Some(table);
                table = unquote(tokens[i + 1].text(src));
                i += 2;
            }
            if tokens.get(i).is_some_and(|t| upper(t, src) == "AS") {
                i += 1;
            }
            let alias = tokens.get(i).filter(|t| is_ident(t, src)).map(|t| {
                i += 1;
                unquote(t.text(src))
            });
            refs.push(TableRef {
                database,
                table,
                alias,
            });
            // `FROM a, b` lists more tables.
            if tokens.get(i).is_some_and(|t| t.text(src) == ",") {
                i += 1;
            } else {
                break;
            }
        }
    }
    refs
}

/// What the clause expects at the end of `head` (the tokens before the word being typed).
///
/// A table name is expected right after `FROM`/`JOIN`/`UPDATE`/`INTO`/..., after a comma in
/// such a table list, or after `db.` in that position. After a complete table name (and
/// optional alias) the next word is a keyword such as `WHERE`, `JOIN` or `UNION`, so that is an
/// expression position.
fn clause_at(head: &[Token], src: &str) -> Expect {
    let keyword_expect = |tok: &Token| {
        let word = upper(tok, src);
        if tok.kind != TokenKind::Word {
            None
        } else if TABLE_KEYWORDS.contains(&word.as_str()) {
            Some(Expect::Table)
        } else if word == "USE" || word == "DATABASE" {
            Some(Expect::Database)
        } else {
            None
        }
    };
    let mut rest = head;
    // `db.` (or `alias.`): what matters is what precedes the qualifier.
    if let [before @ .., q, dot] = rest
        && dot.text(src) == "."
        && is_ident(q, src)
    {
        rest = before;
        match rest.last() {
            Some(t) if t.text(src) == "," => {}
            Some(t) => return keyword_expect(t).unwrap_or(Expect::Expression),
            None => return Expect::Expression,
        }
    }
    match rest.last() {
        None => Expect::Expression,
        Some(t) if t.text(src) == "," => {
            // A list: walk back over its items to the keyword that started it.
            rest.iter()
                .rev()
                .find(|t| {
                    let text = t.text(src);
                    !(is_ident(t, src) || text == "." || text == "," || upper(t, src) == "AS")
                })
                .and_then(keyword_expect)
                .filter(|e| *e == Expect::Table)
                .unwrap_or(Expect::Expression)
        }
        Some(t) => keyword_expect(t).unwrap_or(Expect::Expression),
    }
}

/// Analyzes the statement containing byte offset `cursor` of `text`.
pub fn analyze(text: &str, cursor: usize) -> Analysis {
    let cursor = cursor.min(text.len());
    // Work on the statement around the cursor only.
    let (stmt, local) = split_statements(text)
        .into_iter()
        .find(|s| s.span.start <= cursor && cursor <= s.span.end)
        .map_or((text, cursor), |s| {
            (&text[s.span.clone()], cursor - s.span.start)
        });
    let before = &stmt[..local];

    let prefix_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '$')
        .last()
        .map_or(before.len(), |(i, _)| i);
    let prefix = before[prefix_start..].to_owned();

    let head: Vec<Token> = tokenize(&before[..prefix_start])
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let head_src = &before[..prefix_start];
    let qualifier = match head.as_slice() {
        [.., q, dot] if dot.text(head_src) == "." && is_ident(q, head_src) => {
            Some(unquote(q.text(head_src)))
        }
        _ => None,
    };

    let expect = clause_at(&head, head_src);

    let all: Vec<Token> = tokenize(stmt)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let tables = table_refs(stmt, &all)
        .into_iter()
        // The word being typed isn't a table yet.
        .filter(|r| !(r.alias.is_none() && r.table == prefix && !prefix.is_empty()))
        .collect();

    Analysis {
        prefix,
        qualifier,
        expect,
        tables,
    }
}

impl Analysis {
    /// The table a qualifier refers to, by alias first, then by name.
    fn qualified_table(&self, qualifier: &str) -> Option<&TableRef> {
        self.tables
            .iter()
            .find(|r| {
                r.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(qualifier))
            })
            .or_else(|| {
                self.tables
                    .iter()
                    .find(|r| r.table.eq_ignore_ascii_case(qualifier))
            })
    }

    fn resolve(r: &TableRef, default_db: Option<&str>) -> Option<(String, String)> {
        let db = r
            .database
            .clone()
            .or_else(|| default_db.map(str::to_owned))?;
        Some((db, r.table.clone()))
    }

    /// The tables whose columns are offered, as (database, table).
    fn column_sources(&self, default_db: Option<&str>) -> Vec<(String, String)> {
        match &self.qualifier {
            Some(q) => self
                .qualified_table(q)
                .and_then(|r| Self::resolve(r, default_db))
                .into_iter()
                .collect(),
            None if self.expect == Expect::Expression => self
                .tables
                .iter()
                .filter_map(|r| Self::resolve(r, default_db))
                .collect(),
            None => Vec::new(),
        }
    }
}

impl Catalog {
    /// What has to be loaded before [`suggest`] can answer `analysis`.
    pub fn missing(&self, analysis: &Analysis, default_db: Option<&str>) -> Missing {
        let mut missing = Missing::default();
        let mut want_objects = BTreeSet::new();
        match (&analysis.qualifier, analysis.expect) {
            (_, Expect::Database) => missing.databases = self.databases.is_none(),
            // `db.` in a table position: that database's tables.
            (Some(q), Expect::Table) => {
                want_objects.insert(q.clone());
            }
            (None, Expect::Table) => {
                missing.databases = self.databases.is_none();
                want_objects.extend(default_db.map(str::to_owned));
            }
            (Some(q), Expect::Expression) if analysis.qualified_table(q).is_none() => {
                // Not an alias or table of this statement: maybe `db.table`.
                want_objects.insert(q.clone());
            }
            (_, Expect::Expression) => {
                if analysis.tables.is_empty() {
                    want_objects.extend(default_db.map(str::to_owned));
                }
            }
        }
        missing.objects = want_objects
            .into_iter()
            .filter(|db| !self.objects.contains_key(db))
            .collect();
        missing.columns = analysis
            .column_sources(default_db)
            .into_iter()
            .filter(|key| !self.columns.contains_key(key))
            .collect();
        missing
    }
}

fn quote_if_needed(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        && !is_keyword(name);
    if plain {
        name.to_owned()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

fn matches(prefix: &str, candidate: &str) -> bool {
    candidate.to_lowercase().starts_with(&prefix.to_lowercase())
}

/// Suggestions for `analysis`, from whatever the catalog holds.
pub fn suggest(
    analysis: &Analysis,
    catalog: &Catalog,
    default_db: Option<&str>,
) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let mut push = |label: &str, kind: SuggestionKind, detail: String| {
        if matches(&analysis.prefix, label) {
            out.push(Suggestion {
                insert: match kind {
                    SuggestionKind::Keyword | SuggestionKind::Function => label.to_owned(),
                    _ => quote_if_needed(label),
                },
                label: label.to_owned(),
                kind,
                detail,
            });
        }
    };
    let objects_of = |db: &str| {
        catalog
            .objects
            .get(db)
            .map(Vec::as_slice)
            .unwrap_or_default()
    };

    // Columns of the referenced tables (or the qualified one).
    for (db, table) in analysis.column_sources(default_db) {
        for (column, column_type) in catalog
            .columns
            .get(&(db.clone(), table.clone()))
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            push(
                column,
                SuggestionKind::Column,
                format!("{column_type} · {table}"),
            );
        }
    }

    match (&analysis.qualifier, analysis.expect) {
        (_, Expect::Database) => {
            for db in catalog.databases.iter().flatten() {
                push(db, SuggestionKind::Database, String::new());
            }
        }
        (Some(q), _) => {
            // `db.` → that database's tables, unless it named a table of this statement.
            if analysis.qualified_table(q).is_none() || analysis.expect == Expect::Table {
                for (name, view) in objects_of(q) {
                    let kind = if *view {
                        SuggestionKind::View
                    } else {
                        SuggestionKind::Table
                    };
                    push(name, kind, q.clone());
                }
            }
        }
        (None, Expect::Table) => {
            for (name, view) in default_db.map(objects_of).unwrap_or_default() {
                let kind = if *view {
                    SuggestionKind::View
                } else {
                    SuggestionKind::Table
                };
                push(name, kind, default_db.unwrap_or_default().to_owned());
            }
            for db in catalog.databases.iter().flatten() {
                push(db, SuggestionKind::Database, String::new());
            }
        }
        (None, Expect::Expression) => {
            if analysis.tables.is_empty() {
                for (name, view) in default_db.map(objects_of).unwrap_or_default() {
                    let kind = if *view {
                        SuggestionKind::View
                    } else {
                        SuggestionKind::Table
                    };
                    push(name, kind, default_db.unwrap_or_default().to_owned());
                }
            }
            for f in FUNCTIONS {
                push(f, SuggestionKind::Function, "function".into());
            }
            for k in KEYWORDS {
                push(k, SuggestionKind::Keyword, String::new());
            }
        }
    }
    // Same name from several tables: keep the first.
    let mut seen = BTreeSet::new();
    out.retain(|s| seen.insert((s.kind, s.label.clone())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_end(sql: &str) -> Analysis {
        analyze(sql, sql.len())
    }

    fn catalog() -> Catalog {
        let mut c = Catalog {
            databases: Some(vec!["shop".into(), "logs".into()]),
            ..Catalog::default()
        };
        c.objects.insert(
            "shop".into(),
            vec![
                ("orders".into(), false),
                ("order_items".into(), false),
                ("order totals".into(), true),
            ],
        );
        c.columns.insert(
            ("shop".into(), "orders".into()),
            vec![
                ("id".into(), "int".into()),
                ("status".into(), "enum".into()),
            ],
        );
        c.columns.insert(
            ("shop".into(), "customers".into()),
            vec![
                ("id".into(), "int".into()),
                ("name".into(), "varchar".into()),
            ],
        );
        c
    }

    fn labels(s: &[Suggestion]) -> Vec<&str> {
        s.iter().map(|s| s.label.as_str()).collect()
    }

    #[test]
    fn detects_table_position() {
        let a = at_end("SELECT * FROM ord");
        assert_eq!(a.expect, Expect::Table);
        assert_eq!(a.prefix, "ord");
        let a = at_end("select * from orders o join ");
        assert_eq!(a.expect, Expect::Table);
        assert_eq!(at_end("USE sh").expect, Expect::Database);
        assert_eq!(at_end("select * from a, b, ").expect, Expect::Table);
    }

    #[test]
    fn keywords_follow_a_complete_table_name() {
        for sql in [
            "SELECT * FROM orders WH",
            "SELECT * FROM orders o WH",
            "select * from shop.orders WH",
            "select * from a, b WH",
        ] {
            let a = at_end(sql);
            assert_eq!(a.expect, Expect::Expression, "{sql}");
            let s = suggest(&a, &catalog(), Some("shop"));
            assert!(labels(&s).contains(&"WHERE"), "{sql}");
        }
        let a = at_end("SELECT 1 FROM orders UN");
        assert!(labels(&suggest(&a, &catalog(), Some("shop"))).contains(&"UNION"));
        // Still a table position after JOIN, a comma in the list, or `db.`.
        assert_eq!(at_end("select * from orders o join ").expect, Expect::Table);
        assert_eq!(at_end("select * from orders, ").expect, Expect::Table);
        assert_eq!(at_end("select * from shop.").expect, Expect::Table);
        // A comma in the SELECT list is not a table list.
        assert_eq!(at_end("select a, ").expect, Expect::Expression);
        // `alias.` in an expression stays an expression.
        assert_eq!(
            at_end("select * from orders o where o.").expect,
            Expect::Expression
        );
    }

    #[test]
    fn resolves_aliases_and_qualifiers() {
        let a = analyze(
            "SELECT o. FROM shop.orders AS o JOIN customers c ON c.id = o.customer_id",
            9,
        );
        assert_eq!(a.qualifier.as_deref(), Some("o"));
        assert_eq!(a.prefix, "");
        assert_eq!(
            a.tables,
            [
                TableRef {
                    database: Some("shop".into()),
                    table: "orders".into(),
                    alias: Some("o".into()),
                },
                TableRef {
                    database: None,
                    table: "customers".into(),
                    alias: Some("c".into()),
                },
            ]
        );
        let s = suggest(&a, &catalog(), Some("shop"));
        assert_eq!(labels(&s), ["id", "status"]);
    }

    #[test]
    fn suggests_tables_and_quotes_odd_names() {
        let a = at_end("select * from ord");
        let s = suggest(&a, &catalog(), Some("shop"));
        assert_eq!(labels(&s), ["orders", "order_items", "order totals"]);
        assert_eq!(s[2].insert, "`order totals`");
        assert_eq!(s[2].kind, SuggestionKind::View);
    }

    #[test]
    fn expression_mixes_columns_functions_keywords() {
        let a = at_end("SELECT * FROM orders WHERE st");
        let s = suggest(&a, &catalog(), Some("shop"));
        let l = labels(&s);
        assert_eq!(l[0], "status");
        assert!(l.contains(&"STDDEV") && l.contains(&"STARTING"));
    }

    #[test]
    fn only_the_current_statement_counts() {
        let sql = "select * from customers;\nselect  from orders";
        let a = analyze(sql, sql.find("select  ").unwrap() + 7);
        assert_eq!(a.tables.len(), 1);
        assert_eq!(a.tables[0].table, "orders");
    }

    #[test]
    fn reports_missing_schema_data() {
        let empty = Catalog::default();
        let a = at_end("select o. from orders o");
        // Cursor right after "o." in a statement whose FROM comes later.
        let a2 = analyze("select o. from orders o", 9);
        assert_eq!(
            empty.missing(&a2, Some("shop")).columns,
            [("shop".to_owned(), "orders".to_owned())]
        );
        assert!(!empty.missing(&a, Some("shop")).is_empty());
        let full = catalog();
        assert!(
            full.missing(&at_end("select * from ord"), Some("shop"))
                .is_empty()
        );
        // `logs.` in table position needs that database's objects.
        assert_eq!(
            full.missing(&at_end("select * from logs."), Some("shop"))
                .objects,
            ["logs".to_owned()]
        );
    }
}
