//! The table designer's model: a table's columns as the user edits them, and the
//! `CREATE TABLE` / `ALTER TABLE` that applies them.

use super::dialect::Dialect;
use crate::schema::{CheckInfo, ColumnInfo, ForeignKeyInfo, IndexInfo, TableOptions};

/// One column in the designer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnDef {
    pub name: String,
    /// Full SQL type, e.g. `VARCHAR(100)` or `INT UNSIGNED`.
    pub data_type: String,
    pub nullable: bool,
    /// Default as SQL, e.g. `0`, `'n/a'` or `CURRENT_TIMESTAMP`; empty for none.
    pub default_sql: String,
    pub auto_increment: bool,
    pub primary_key: bool,
    pub comment: String,
    /// Kept from the existing column so changing it doesn't reset it to the table default;
    /// empty for the default.
    pub collation: String,
    /// A generated (computed) column. Its expression isn't loaded, so it can be kept or
    /// dropped but not changed.
    pub generated: bool,
    /// The column's name in the existing table; `None` for a new column.
    pub original_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum IndexKind {
    #[default]
    Index,
    Unique,
    Fulltext,
    Spatial,
}

impl IndexKind {
    pub const ALL: [Self; 4] = [Self::Index, Self::Unique, Self::Fulltext, Self::Spatial];

    pub fn label(self) -> &'static str {
        match self {
            Self::Index => "INDEX",
            Self::Unique => "UNIQUE",
            Self::Fulltext => "FULLTEXT",
            Self::Spatial => "SPATIAL",
        }
    }
}

/// A secondary index (the primary key is kept on the columns).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexDef {
    pub name: String,
    pub kind: IndexKind,
    /// Column names, optionally with a prefix length: `name` or `name(10)`.
    pub columns: Vec<String>,
    /// The index's name in the existing table; `None` for a new index.
    pub original_name: Option<String>,
}

/// What a foreign key does when the referenced row is updated or deleted.
pub const REFERENTIAL_ACTIONS: [&str; 5] = [
    "RESTRICT",
    "CASCADE",
    "SET NULL",
    "NO ACTION",
    "SET DEFAULT",
];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ForeignKeyDef {
    pub name: String,
    pub columns: Vec<String>,
    /// Empty for the table's own database.
    pub ref_database: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
    /// The constraint's name in the existing table; `None` for a new one.
    pub original_name: Option<String>,
}

/// A table-level `CHECK` constraint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckDef {
    pub name: String,
    /// The expression, e.g. `price >= 0`.
    pub clause: String,
    /// The constraint's name in the existing table; `None` for a new one.
    pub original_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableDesign {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    pub checks: Vec<CheckDef>,
    /// SQLite: the table's triggers as `CREATE TRIGGER` statements, re-created after the
    /// table is rebuilt.
    pub triggers: Vec<String>,
    /// The primary key constraint's own name, when the server has one (PostgreSQL).
    pub primary_key_name: String,
    /// SQLite: what follows the column list in the table's `CREATE`, e.g. `WITHOUT ROWID` or
    /// `STRICT`, kept when the table is rebuilt.
    pub suffix: String,
    /// Empty fields leave the server default (new tables) or the current value alone.
    pub options: TableOptions,
}

/// `ENGINE=… DEFAULT CHARSET=… COLLATE=… COMMENT='…'` for the options that are set.
fn options_sql(d: Dialect, options: &TableOptions) -> Vec<String> {
    let mut out = Vec::new();
    if !options.engine.trim().is_empty() {
        out.push(format!("ENGINE={}", options.engine.trim()));
    }
    let collation = options.collation.trim();
    if !collation.is_empty() {
        let charset = collation.split('_').next().unwrap_or(collation);
        out.push(format!("DEFAULT CHARSET={charset} COLLATE={collation}"));
    }
    if !options.comment.is_empty() {
        out.push(format!("COMMENT={}", d.quote_str(&options.comment)));
    }
    out
}

/// `CONSTRAINT name FOREIGN KEY (a) REFERENCES t (b) ON DELETE … ON UPDATE …`.
pub fn foreign_key_sql(d: Dialect, db: &str, fk: &ForeignKeyDef) -> String {
    let list = |cols: &[String]| {
        cols.iter()
            .map(|c| d.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ref_db = if fk.ref_database.trim().is_empty() {
        db
    } else {
        fk.ref_database.trim()
    };
    // SQLite foreign keys can only reference tables in their own schema, unqualified.
    let target = match d {
        Dialect::MySql | Dialect::Postgres => d.qualified(ref_db, fk.ref_table.trim()),
        Dialect::Sqlite => d.quote_ident(fk.ref_table.trim()),
    };
    let mut sql = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {target} ({})",
        d.quote_ident(&fk.name),
        list(&fk.columns),
        list(&fk.ref_columns)
    );
    for (clause, action) in [("ON DELETE", &fk.on_delete), ("ON UPDATE", &fk.on_update)] {
        if !action.trim().is_empty() {
            sql.push(' ');
            sql.push_str(clause);
            sql.push(' ');
            sql.push_str(&action.trim().to_ascii_uppercase());
        }
    }
    sql
}

/// `col` or `col(10)` quoted as `` `col` `` / `` `col`(10) ``.
fn index_column_sql(d: Dialect, column: &str) -> String {
    let column = column.trim();
    match column.split_once('(') {
        Some((name, rest)) if rest.ends_with(')') => {
            format!("{}({rest}", d.quote_ident(name.trim()))
        }
        _ => d.quote_ident(column),
    }
}

/// The column an index part refers to, without any prefix length.
fn index_column_name(column: &str) -> &str {
    column
        .split_once('(')
        .map_or(column, |(name, _)| name)
        .trim()
}

/// `UNIQUE INDEX name (a, b)` as used in `CREATE TABLE` and `ADD`.
pub fn index_sql(d: Dialect, index: &IndexDef) -> String {
    let cols: Vec<String> = index
        .columns
        .iter()
        .map(|c| index_column_sql(d, c))
        .collect();
    let kind = match index.kind {
        IndexKind::Index => "INDEX".to_owned(),
        other => format!("{} INDEX", other.label()),
    };
    format!(
        "{kind} {} ({})",
        d.quote_ident(&index.name),
        cols.join(", ")
    )
}

/// Defaults that are expressions rather than literal values.
fn is_expression_default(value: &str) -> bool {
    let upper = value.to_ascii_uppercase();
    upper.starts_with("CURRENT_TIMESTAMP")
        || upper == "NULL"
        || upper.ends_with(')')
        || value.parse::<f64>().is_ok()
        || value.starts_with('\'')
}

impl TableDesign {
    /// Loads an existing table's design from its columns and indexes.
    pub fn from_table(
        d: Dialect,
        name: &str,
        columns: &[ColumnInfo],
        indexes: &[IndexInfo],
    ) -> Self {
        let primary: Vec<&String> = indexes
            .iter()
            .find(|i| i.is_primary())
            .map(|i| i.columns.iter().collect())
            .unwrap_or_default();
        let primary_key_name = indexes
            .iter()
            .find(|i| i.is_primary())
            .map(|i| i.constraint_name.clone())
            .unwrap_or_default();
        let columns = columns
            .iter()
            .map(|c| {
                let extra = c.extra.to_ascii_lowercase();
                ColumnDef {
                    name: c.name.clone(),
                    data_type: c.column_type.clone(),
                    nullable: c.nullable,
                    default_sql: match &c.default {
                        None => String::new(),
                        Some(v) if is_expression_default(v) => v.clone(),
                        Some(v) => d.quote_str(v),
                    },
                    auto_increment: extra.contains("auto_increment"),
                    primary_key: primary.contains(&&c.name),
                    comment: c.comment.clone(),
                    collation: c.collation.clone(),
                    generated: extra.contains("generated"),
                    original_name: Some(c.name.clone()),
                }
            })
            .collect();
        let indexes = indexes
            .iter()
            .filter(|i| !i.is_primary())
            .map(|i| IndexDef {
                name: i.name.clone(),
                kind: match i.index_type.to_ascii_uppercase().as_str() {
                    "FULLTEXT" => IndexKind::Fulltext,
                    "SPATIAL" => IndexKind::Spatial,
                    _ if i.unique => IndexKind::Unique,
                    _ => IndexKind::Index,
                },
                columns: i
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(n, c)| match i.prefix_lengths.get(n).copied().flatten() {
                        Some(len) => format!("{c}({len})"),
                        None => c.clone(),
                    })
                    .collect(),
                original_name: Some(i.name.clone()),
            })
            .collect();
        Self {
            name: name.to_owned(),
            columns,
            indexes,
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            primary_key_name,
            triggers: Vec::new(),
            suffix: String::new(),
            options: TableOptions::default(),
        }
    }

    /// Adds what a SQLite rebuild must carry over: the table's triggers and the options
    /// after its column list in `create` (its `CREATE TABLE` statement).
    #[must_use]
    pub fn with_sqlite_parts(mut self, create: &str, triggers: Vec<String>) -> Self {
        // The table's DDL may be followed by its indexes; only the CREATE TABLE matters here.
        let statements = super::split_statements_for(create, Dialect::Sqlite);
        let create = statements.first().map_or(create, |s| s.sql.as_str());
        let tail = create.rfind(')').map_or("", |at| &create[at + 1..]);
        tail.trim()
            .trim_end_matches(';')
            .trim()
            .clone_into(&mut self.suffix);
        self.triggers = triggers;
        self
    }

    /// Changes `validate` can't see: generated columns, whose expression isn't loaded, can't
    /// be changed, and SQLite rebuilds can't keep them at all.
    pub fn validate_change(&self, d: Dialect, before: &TableDesign) -> Result<(), String> {
        if d == Dialect::Postgres {
            // PostgreSQL keeps column order: existing columns stay in place and new ones go
            // at the end.
            let kept: Vec<&str> = self
                .columns
                .iter()
                .filter_map(|c| c.original_name.as_deref())
                .collect();
            let old: Vec<&str> = before
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .filter(|n| kept.contains(n))
                .collect();
            if kept != old {
                return Err("PostgreSQL can't reorder a table's columns.".into());
            }
            let first_new = self.columns.iter().position(|c| c.original_name.is_none());
            if let Some(first_new) = first_new
                && self.columns[first_new..]
                    .iter()
                    .any(|c| c.original_name.is_some())
            {
                return Err("PostgreSQL adds new columns at the end of the table.".into());
            }
        }
        for old in before.columns.iter().filter(|c| c.generated) {
            let new = self
                .columns
                .iter()
                .find(|c| c.original_name.as_deref() == Some(old.name.as_str()));
            let Some(new) = new else { continue };
            if d == Dialect::Sqlite {
                return Err(format!(
                    "{} is a generated column, which a SQLite table can't keep when it's \
                     rebuilt. Edit the table's CREATE statement instead.",
                    old.name
                ));
            }
            if new != old {
                return Err(format!(
                    "{} is a generated column; it can be kept or dropped, not changed.",
                    old.name
                ));
            }
        }
        Ok(())
    }

    /// `validate`, plus the limits of `d`.
    pub fn validate_for(&self, d: Dialect) -> Result<(), String> {
        self.validate()?;
        let partitioning = self.options.partitioning.trim();
        if !partitioning.is_empty() {
            if !partitioning
                .get(..12)
                .is_some_and(|start| start.eq_ignore_ascii_case("PARTITION BY"))
            {
                return Err("Partitioning starts with PARTITION BY.".into());
            }
            if !self.foreign_keys.is_empty() {
                return Err("A partitioned table can't have foreign keys.".into());
            }
        }
        if d == Dialect::Postgres {
            for index in &self.indexes {
                if !matches!(index.kind, IndexKind::Index | IndexKind::Unique) {
                    return Err(format!(
                        "Use a GIN or GiST index in SQL for {} search ({}).",
                        index.kind.label().to_lowercase(),
                        index.name
                    ));
                }
                if index.columns.iter().any(|c| c.contains('(')) {
                    return Err(format!(
                        "PostgreSQL indexes have no prefix lengths ({}).",
                        index.name
                    ));
                }
            }
        }
        if d == Dialect::Sqlite {
            for c in self.columns.iter().filter(|c| c.auto_increment) {
                let single = self.primary_key() == [c.name.as_str()];
                if !single || !c.data_type.trim().eq_ignore_ascii_case("INTEGER") {
                    return Err(format!(
                        "In SQLite only a lone INTEGER primary key can auto-increment ({}).",
                        c.name
                    ));
                }
            }
            for index in &self.indexes {
                if !matches!(index.kind, IndexKind::Index | IndexKind::Unique) {
                    return Err(format!(
                        "SQLite has no {} indexes ({}).",
                        index.kind.label(),
                        index.name
                    ));
                }
                if index.columns.iter().any(|c| c.contains('(')) {
                    return Err(format!(
                        "SQLite indexes have no prefix lengths ({}).",
                        index.name
                    ));
                }
            }
        }
        Ok(())
    }
    #[must_use]
    pub fn with_checks(mut self, checks: &[CheckInfo]) -> Self {
        self.checks = checks
            .iter()
            .map(|c| CheckDef {
                name: c.name.clone(),
                clause: c.clause.clone(),
                original_name: Some(c.name.clone()),
            })
            .collect();
        self
    }

    /// Adds the table's foreign keys and options; `db` is the table's own database, so
    /// references within it are shown without a database name.
    #[must_use]
    pub fn with_constraints(
        mut self,
        db: &str,
        foreign_keys: &[ForeignKeyInfo],
        options: TableOptions,
    ) -> Self {
        self.foreign_keys = foreign_keys
            .iter()
            .map(|fk| ForeignKeyDef {
                name: fk.name.clone(),
                columns: fk.columns.clone(),
                ref_database: if fk.ref_database == db {
                    String::new()
                } else {
                    fk.ref_database.clone()
                },
                ref_table: fk.ref_table.clone(),
                ref_columns: fk.ref_columns.clone(),
                on_update: fk.on_update.clone(),
                on_delete: fk.on_delete.clone(),
                original_name: Some(fk.name.clone()),
            })
            .collect();
        self.options = options;
        self
    }

    /// Points index columns at renamed columns: an index that names a column by its old name
    /// follows the rename, as the server does.
    pub fn follow_renames(&mut self) {
        let renames: Vec<(String, String)> = self
            .columns
            .iter()
            .filter_map(|c| {
                let old = c.original_name.as_ref()?;
                (old != &c.name).then(|| (old.clone(), c.name.clone()))
            })
            .collect();
        let current: Vec<String> = self.columns.iter().map(|c| c.name.to_lowercase()).collect();
        // Checks name columns inside an expression; quoted names are renamed there.
        for check in &mut self.checks {
            for (old, new) in &renames {
                let quoted = Dialect::MySql.quote_ident(old);
                if check.clause.contains(&quoted) && !current.contains(&old.to_lowercase()) {
                    check.clause = check
                        .clause
                        .replace(&quoted, &Dialect::MySql.quote_ident(new));
                }
            }
        }
        for fk in &mut self.foreign_keys {
            for column in &mut fk.columns {
                if current.contains(&column.to_lowercase()) {
                    continue;
                }
                if let Some((_, new)) = renames.iter().find(|(old, _)| old == column) {
                    column.clone_from(new);
                }
            }
        }
        for index in &mut self.indexes {
            for column in &mut index.columns {
                let name = index_column_name(column).to_owned();
                if current.contains(&name.to_lowercase()) {
                    continue;
                }
                if let Some((_, new)) = renames.iter().find(|(old, _)| *old == name) {
                    *column = column.replacen(&name, new, 1);
                }
            }
        }
    }

    fn primary_key(&self) -> Vec<&str> {
        self.columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.as_str())
            .collect()
    }

    /// Problems that would make the SQL fail, in plain words.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("The table needs a name.".into());
        }
        if self.columns.is_empty() {
            return Err("The table needs at least one column.".into());
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.columns {
            if c.name.trim().is_empty() {
                return Err("Every column needs a name.".into());
            }
            if c.data_type.trim().is_empty() {
                return Err(format!("Column {} needs a type.", c.name));
            }
            if !seen.insert(c.name.to_lowercase()) {
                return Err(format!("Column {} appears twice.", c.name));
            }
        }
        if self.columns.iter().filter(|c| c.auto_increment).count() > 1 {
            return Err("Only one column can be AUTO_INCREMENT.".into());
        }
        let mut index_names = std::collections::HashSet::new();
        for index in &self.indexes {
            if index.name.trim().is_empty() {
                return Err("Every index needs a name.".into());
            }
            if index.name.eq_ignore_ascii_case("PRIMARY") {
                return Err("Mark primary key columns with PK instead of an index.".into());
            }
            if !index_names.insert(index.name.to_lowercase()) {
                return Err(format!("Index {} appears twice.", index.name));
            }
            if index.columns.is_empty() {
                return Err(format!("Index {} needs at least one column.", index.name));
            }
            for column in &index.columns {
                let name = index_column_name(column);
                if !seen.contains(&name.to_lowercase()) {
                    return Err(format!("Index {} uses unknown column {name}.", index.name));
                }
            }
        }
        let mut check_names = std::collections::HashSet::new();
        for check in &self.checks {
            if check.name.trim().is_empty() {
                return Err("Every check needs a name.".into());
            }
            if !check_names.insert(check.name.to_lowercase()) {
                return Err(format!("Check {} appears twice.", check.name));
            }
            if check.clause.trim().is_empty() {
                return Err(format!("Check {} needs an expression.", check.name));
            }
        }
        let mut fk_names = std::collections::HashSet::new();
        for fk in &self.foreign_keys {
            if fk.name.trim().is_empty() {
                return Err("Every foreign key needs a name.".into());
            }
            if !fk_names.insert(fk.name.to_lowercase()) {
                return Err(format!("Foreign key {} appears twice.", fk.name));
            }
            if fk.columns.is_empty() {
                return Err(format!(
                    "Foreign key {} needs at least one column.",
                    fk.name
                ));
            }
            if let Some(c) = fk
                .columns
                .iter()
                .find(|c| !seen.contains(&c.to_lowercase()))
            {
                return Err(format!("Foreign key {} uses unknown column {c}.", fk.name));
            }
            if fk.ref_table.trim().is_empty() {
                return Err(format!("Foreign key {} needs a referenced table.", fk.name));
            }
            if fk.ref_columns.len() != fk.columns.len() {
                return Err(format!(
                    "Foreign key {} needs as many referenced columns as columns.",
                    fk.name
                ));
            }
            for action in [&fk.on_update, &fk.on_delete] {
                let action = action.trim();
                if !action.is_empty()
                    && !REFERENTIAL_ACTIONS
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(action))
                {
                    return Err(format!("Foreign key {}: unknown action {action}.", fk.name));
                }
            }
        }
        Ok(())
    }
}

/// `name TYPE [NOT NULL] [DEFAULT x] [AUTO_INCREMENT] [COMMENT '...']`.
pub fn column_sql(d: Dialect, c: &ColumnDef) -> String {
    match d {
        Dialect::Sqlite => return sqlite_column_sql(c, false),
        Dialect::Postgres => return postgres_column_sql(c),
        Dialect::MySql => {}
    }
    let mut sql = format!("{} {}", d.quote_ident(&c.name), c.data_type.trim());
    if !c.collation.trim().is_empty() {
        sql.push_str(" COLLATE ");
        sql.push_str(c.collation.trim());
    }
    sql.push_str(if c.nullable && !c.primary_key {
        " NULL"
    } else {
        " NOT NULL"
    });
    if !c.default_sql.trim().is_empty() {
        sql.push_str(" DEFAULT ");
        sql.push_str(c.default_sql.trim());
    }
    if c.auto_increment {
        sql.push_str(" AUTO_INCREMENT");
    }
    if !c.comment.is_empty() {
        sql.push_str(" COMMENT ");
        sql.push_str(&d.quote_str(&c.comment));
    }
    sql
}

/// A PostgreSQL column: auto-increment is an identity column.
fn postgres_column_sql(c: &ColumnDef) -> String {
    let d = Dialect::Postgres;
    let mut sql = format!("{} {}", d.quote_ident(&c.name), c.data_type.trim());
    if !c.collation.trim().is_empty() {
        sql.push_str(" COLLATE ");
        sql.push_str(&d.quote_ident(c.collation.trim()));
    }
    if c.auto_increment {
        sql.push_str(" GENERATED BY DEFAULT AS IDENTITY");
    } else if !c.default_sql.trim().is_empty() {
        sql.push_str(" DEFAULT ");
        sql.push_str(c.default_sql.trim());
    }
    if !c.nullable || c.primary_key {
        sql.push_str(" NOT NULL");
    }
    sql
}

/// PostgreSQL's `CREATE INDEX`, `COMMENT ON TABLE` and `COMMENT ON COLUMN` statements for a
/// table: they aren't part of its `CREATE TABLE`.
fn postgres_extra_statements(db: &str, design: &TableDesign, comments: bool) -> Vec<String> {
    let d = Dialect::Postgres;
    let target = d.qualified(db, &design.name);
    let mut out: Vec<String> = design
        .indexes
        .iter()
        .map(|i| postgres_index_sql(db, &design.name, i))
        .collect();
    if comments {
        if !design.options.comment.is_empty() {
            out.push(format!(
                "COMMENT ON TABLE {target} IS {}",
                d.quote_str(&design.options.comment)
            ));
        }
        for c in design.columns.iter().filter(|c| !c.comment.is_empty()) {
            out.push(format!(
                "COMMENT ON COLUMN {target}.{} IS {}",
                d.quote_ident(&c.name),
                d.quote_str(&c.comment)
            ));
        }
    }
    out
}

fn postgres_index_sql(db: &str, table: &str, index: &IndexDef) -> String {
    let d = Dialect::Postgres;
    let cols: Vec<String> = index
        .columns
        .iter()
        .map(|c| d.quote_ident(c.trim()))
        .collect();
    format!(
        "CREATE {}INDEX {} ON {} ({})",
        if index.kind == IndexKind::Unique {
            "UNIQUE "
        } else {
            ""
        },
        d.quote_ident(&index.name),
        d.qualified(db, table),
        cols.join(", ")
    )
}

/// A SQLite column; `inline_key` makes it `PRIMARY KEY AUTOINCREMENT`.
fn sqlite_column_sql(c: &ColumnDef, inline_key: bool) -> String {
    let d = Dialect::Sqlite;
    let mut sql = format!("{} {}", d.quote_ident(&c.name), c.data_type.trim());
    if inline_key {
        sql.push_str(" PRIMARY KEY AUTOINCREMENT");
    }
    if !c.nullable || c.primary_key {
        sql.push_str(" NOT NULL");
    }
    if !c.collation.trim().is_empty() {
        sql.push_str(" COLLATE ");
        sql.push_str(c.collation.trim());
    }
    if !c.default_sql.trim().is_empty() {
        sql.push_str(" DEFAULT ");
        sql.push_str(c.default_sql.trim());
    }
    sql
}

/// SQLite's `CREATE TABLE`: keys, foreign keys and checks inline; indexes are separate.
fn sqlite_create_sql(db: &str, design: &TableDesign) -> String {
    let d = Dialect::Sqlite;
    let pk = design.primary_key();
    let auto = pk.len() == 1
        && design
            .columns
            .iter()
            .any(|c| c.name == pk[0] && c.auto_increment);
    let mut lines: Vec<String> = design
        .columns
        .iter()
        .map(|c| format!("  {}", sqlite_column_sql(c, auto && c.name == pk[0])))
        .collect();
    if !pk.is_empty() && !auto {
        let cols: Vec<String> = pk.iter().map(|c| d.quote_ident(c)).collect();
        lines.push(format!("  PRIMARY KEY ({})", cols.join(", ")));
    }
    lines.extend(
        design
            .foreign_keys
            .iter()
            .map(|fk| format!("  {}", foreign_key_sql(d, db, fk))),
    );
    lines.extend(
        design
            .checks
            .iter()
            .map(|c| format!("  {}", check_sql(d, c))),
    );
    let mut sql = format!(
        "CREATE TABLE {} (\n{}\n)",
        d.qualified(db, &design.name),
        lines.join(",\n")
    );
    if !design.suffix.is_empty() {
        sql.push(' ');
        sql.push_str(&design.suffix);
    }
    sql
}

/// SQLite's `CREATE INDEX` statements for a table's secondary indexes.
fn sqlite_index_statements(db: &str, design: &TableDesign) -> Vec<String> {
    let d = Dialect::Sqlite;
    design
        .indexes
        .iter()
        .map(|i| {
            let cols: Vec<String> = i.columns.iter().map(|c| d.quote_ident(c.trim())).collect();
            format!(
                "CREATE {}INDEX {} ON {} ({})",
                if i.kind == IndexKind::Unique {
                    "UNIQUE "
                } else {
                    ""
                },
                d.qualified(db, &i.name),
                d.quote_ident(&design.name),
                cols.join(", ")
            )
        })
        .collect()
}

/// Everything that creates the designed table: one `CREATE TABLE`, plus `CREATE INDEX`
/// statements on SQLite.
pub fn create_statements(d: Dialect, db: &str, design: &TableDesign) -> Vec<String> {
    let mut out = vec![create_sql(d, db, design)];
    match d {
        Dialect::Sqlite => out.extend(sqlite_index_statements(db, design)),
        Dialect::Postgres => out.extend(postgres_extra_statements(db, design, true)),
        Dialect::MySql => {}
    }
    out
}

/// `CREATE TABLE db.name (...)` for a new table.
pub fn create_sql(d: Dialect, db: &str, design: &TableDesign) -> String {
    if d == Dialect::Sqlite {
        return sqlite_create_sql(db, design);
    }
    // PostgreSQL: like MySQL's, but indexes, options and comments are separate statements.
    let inline_indexes = d == Dialect::MySql;
    let mut lines: Vec<String> = design
        .columns
        .iter()
        .map(|c| format!("  {}", column_sql(d, c)))
        .collect();
    let pk = design.primary_key();
    if !pk.is_empty() {
        let cols: Vec<String> = pk.iter().map(|c| d.quote_ident(c)).collect();
        lines.push(format!("  PRIMARY KEY ({})", cols.join(", ")));
    }
    if inline_indexes {
        lines.extend(
            design
                .indexes
                .iter()
                .map(|i| format!("  {}", index_sql(d, i))),
        );
    }
    lines.extend(
        design
            .foreign_keys
            .iter()
            .map(|fk| format!("  {}", foreign_key_sql(d, db, fk))),
    );
    lines.extend(
        design
            .checks
            .iter()
            .map(|c| format!("  {}", check_sql(d, c))),
    );
    let mut sql = format!(
        "CREATE TABLE {} (\n{}\n)",
        d.qualified(db, &design.name),
        lines.join(",\n")
    );
    if d != Dialect::MySql {
        return sql;
    }
    for option in options_sql(d, &design.options) {
        sql.push(' ');
        sql.push_str(&option);
    }
    let partitioning = design.options.partitioning.trim();
    if !partitioning.is_empty() {
        sql.push('\n');
        sql.push_str(partitioning);
    }
    sql
}

/// Whether two `PARTITION BY` clauses differ, other than in spacing.
fn partitioning_changed(before: &str, after: &str) -> bool {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    words(before) != words(after)
}

/// `CONSTRAINT name CHECK (expression)`.
pub fn check_sql(d: Dialect, check: &CheckDef) -> String {
    format!(
        "CONSTRAINT {} CHECK ({})",
        d.quote_ident(&check.name),
        check.clause.trim()
    )
}

/// `after` when it differs from `before` (ignoring case), otherwise empty.
fn changed_text(before: &str, after: &str) -> String {
    if before.trim().eq_ignore_ascii_case(after.trim()) {
        String::new()
    } else {
        after.trim().to_owned()
    }
}

/// The `ALTER TABLE` statements turning `before` into `after`; none when nothing changed.
/// Columns are matched by `original_name`, so renames keep their data. It's one statement,
/// plus a second when a foreign key is re-created under its old name (InnoDB won't drop and
/// add the same constraint name in one statement).
pub fn alter_sql(d: Dialect, db: &str, before: &TableDesign, after: &TableDesign) -> Vec<String> {
    match d {
        Dialect::MySql => alter_mysql(d, db, before, after),
        Dialect::Sqlite => alter_sqlite(db, before, after),
        Dialect::Postgres => alter_postgres(db, before, after),
    }
}

/// SQLite's ALTER TABLE can only rename and add or drop simple columns, so any other change
/// rebuilds the table: create the new design under a temporary name, copy the rows, drop the
/// old table, rename the new one, and re-create its indexes and triggers. Runs with foreign
/// key enforcement off, in one transaction.
fn alter_sqlite(db: &str, before: &TableDesign, after: &TableDesign) -> Vec<String> {
    let d = Dialect::Sqlite;
    // The engine-neutral comparison tells whether anything changed at all.
    if alter_mysql(Dialect::MySql, db, before, after).is_empty() {
        return Vec::new();
    }
    let renamed_only = TableDesign {
        name: before.name.clone(),
        ..after.clone()
    };
    if before.name != after.name
        && alter_mysql(Dialect::MySql, db, before, &renamed_only).is_empty()
    {
        return vec![format!(
            "ALTER TABLE {} RENAME TO {}",
            d.qualified(db, &before.name),
            d.quote_ident(&after.name)
        )];
    }
    let temp = format!("ovs_rebuild_{}", after.name);
    let staged = TableDesign {
        name: temp.clone(),
        ..after.clone()
    };
    let (new_cols, old_cols): (Vec<String>, Vec<String>) = after
        .columns
        .iter()
        .filter_map(|c| {
            let old = c.original_name.as_ref()?;
            before
                .columns
                .iter()
                .any(|b| &b.name == old)
                .then(|| (d.quote_ident(&c.name), d.quote_ident(old)))
        })
        .unzip();
    let mut out = vec![
        "PRAGMA foreign_keys = OFF".to_owned(),
        "BEGIN".to_owned(),
        sqlite_create_sql(db, &staged),
    ];
    if !new_cols.is_empty() {
        out.push(format!(
            "INSERT INTO {} ({}) SELECT {} FROM {}",
            d.qualified(db, &temp),
            new_cols.join(", "),
            old_cols.join(", "),
            d.qualified(db, &before.name)
        ));
    }
    out.push(format!("DROP TABLE {}", d.qualified(db, &before.name)));
    out.push(format!(
        "ALTER TABLE {} RENAME TO {}",
        d.qualified(db, &temp),
        d.quote_ident(&after.name)
    ));
    out.extend(sqlite_index_statements(db, after));
    out.extend(after.triggers.iter().cloned());
    out.push("COMMIT".to_owned());
    out.push("PRAGMA foreign_keys = ON".to_owned());
    out
}

/// PostgreSQL's DDL is transactional, so the whole change runs in one transaction: renames
/// as statements of their own (they can't be combined with other actions), then one
/// `ALTER TABLE` for the rest, then indexes and comments.
fn alter_postgres(db: &str, before: &TableDesign, after: &TableDesign) -> Vec<String> {
    let d = Dialect::Postgres;
    if alter_mysql(Dialect::MySql, db, before, after).is_empty() {
        return Vec::new();
    }
    let old_table = d.qualified(db, &before.name);
    let mut renamed = before.clone();
    renamed.columns.clone_from(&after.columns);
    renamed.follow_renames();
    let mut out = vec!["BEGIN".to_owned()];
    let mut actions: Vec<String> = Vec::new();
    let mut tail: Vec<String> = Vec::new();

    // Constraints and indexes that went away or changed go first.
    for (old, old_renamed) in before.foreign_keys.iter().zip(&renamed.foreign_keys) {
        let new = after
            .foreign_keys
            .iter()
            .find(|fk| fk.original_name.as_deref() == Some(old.name.as_str()));
        let unchanged = new.is_some_and(|new| {
            ForeignKeyDef {
                original_name: None,
                ..new.clone()
            } == ForeignKeyDef {
                original_name: None,
                ..old_renamed.clone()
            }
        });
        if !unchanged {
            actions.push(format!("DROP CONSTRAINT {}", d.quote_ident(&old.name)));
            if let Some(new) = new {
                tail.push(format!("ADD {}", foreign_key_sql(d, db, new)));
            }
        }
    }
    for old in &before.checks {
        let new = after
            .checks
            .iter()
            .find(|c| c.original_name.as_deref() == Some(old.name.as_str()));
        // PostgreSQL rewrites check expressions when a column is renamed, so a check is only
        // re-created when the user changed it.
        let unchanged = new.is_some_and(|new| {
            let mut expected = old.clone();
            let mut probe = renamed.clone();
            probe.checks = vec![expected.clone()];
            probe.follow_renames();
            expected.clause.clone_from(&probe.checks[0].clause);
            new.name == old.name && new.clause.trim() == expected.clause.trim()
        });
        if !unchanged {
            actions.push(format!("DROP CONSTRAINT {}", d.quote_ident(&old.name)));
            if let Some(new) = new {
                tail.push(format!("ADD {}", check_sql(d, new)));
            }
        }
    }
    tail.extend(
        after
            .foreign_keys
            .iter()
            .filter(|fk| fk.original_name.is_none())
            .map(|fk| format!("ADD {}", foreign_key_sql(d, db, fk))),
    );
    tail.extend(
        after
            .checks
            .iter()
            .filter(|c| c.original_name.is_none())
            .map(|c| format!("ADD {}", check_sql(d, c))),
    );
    let mut new_indexes = Vec::new();
    for (old, old_renamed) in before.indexes.iter().zip(&renamed.indexes) {
        let new = after
            .indexes
            .iter()
            .find(|i| i.original_name.as_deref() == Some(old.name.as_str()));
        let unchanged = new.is_some_and(|new| {
            new.name == old.name && new.kind == old.kind && new.columns == old_renamed.columns
        });
        if !unchanged {
            out.push(format!("DROP INDEX {}", d.qualified(db, &old.name)));
            if let Some(new) = new {
                new_indexes.push(new);
            }
        }
    }
    new_indexes.extend(after.indexes.iter().filter(|i| i.original_name.is_none()));

    let before_key: Vec<String> = renamed
        .columns
        .iter()
        .filter(|c| {
            c.original_name
                .as_ref()
                .is_some_and(|o| before.columns.iter().any(|b| &b.name == o && b.primary_key))
        })
        .map(|c| c.name.clone())
        .collect();
    let after_key: Vec<String> = after
        .primary_key()
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    if before_key != after_key {
        if !before.primary_key().is_empty() {
            let name = if before.primary_key_name.is_empty() {
                format!("{}_pkey", before.name)
            } else {
                before.primary_key_name.clone()
            };
            actions.push(format!("DROP CONSTRAINT {}", d.quote_ident(&name)));
        }
        if !after_key.is_empty() {
            let cols: Vec<String> = after_key.iter().map(|c| d.quote_ident(c)).collect();
            tail.insert(0, format!("ADD PRIMARY KEY ({})", cols.join(", ")));
        }
    }

    // Columns: drops, then renames (statements of their own), then changes and additions.
    for old in &before.columns {
        let kept = after
            .columns
            .iter()
            .any(|c| c.original_name.as_deref() == Some(old.name.as_str()));
        if !kept {
            actions.push(format!("DROP COLUMN {}", d.quote_ident(&old.name)));
        }
    }
    if !actions.is_empty() {
        out.push(format!(
            "ALTER TABLE {old_table}\n  {}",
            actions.join(",\n  ")
        ));
        actions.clear();
    }
    for c in &after.columns {
        if let Some(old) = c.original_name.as_deref().filter(|o| *o != c.name) {
            out.push(format!(
                "ALTER TABLE {old_table} RENAME COLUMN {} TO {}",
                d.quote_ident(old),
                d.quote_ident(&c.name)
            ));
        }
    }
    let mut comments = Vec::new();
    for c in &after.columns {
        let col = d.quote_ident(&c.name);
        let Some(old) = c
            .original_name
            .as_ref()
            .and_then(|o| before.columns.iter().find(|b| &b.name == o))
        else {
            actions.push(format!("ADD COLUMN {}", postgres_column_sql(c)));
            if !c.comment.is_empty() {
                comments.push((c.name.clone(), c.comment.clone()));
            }
            continue;
        };
        let type_changed = !old
            .data_type
            .trim()
            .eq_ignore_ascii_case(c.data_type.trim());
        if type_changed || old.collation.trim() != c.collation.trim() {
            let collate = if c.collation.trim().is_empty() {
                String::new()
            } else {
                format!(" COLLATE {}", d.quote_ident(c.collation.trim()))
            };
            actions.push(format!(
                "ALTER COLUMN {col} TYPE {}{collate} USING {col}::{}",
                c.data_type.trim(),
                c.data_type.trim()
            ));
        }
        let old_null = old.nullable && !old.primary_key;
        let new_null = c.nullable && !c.primary_key;
        if old_null && !new_null {
            actions.push(format!("ALTER COLUMN {col} SET NOT NULL"));
        } else if !old_null && new_null {
            actions.push(format!("ALTER COLUMN {col} DROP NOT NULL"));
        }
        if old.auto_increment != c.auto_increment {
            actions.push(if c.auto_increment {
                format!("ALTER COLUMN {col} ADD GENERATED BY DEFAULT AS IDENTITY")
            } else {
                format!("ALTER COLUMN {col} DROP IDENTITY IF EXISTS")
            });
        }
        if old.default_sql.trim() != c.default_sql.trim() && !c.auto_increment {
            actions.push(if c.default_sql.trim().is_empty() {
                format!("ALTER COLUMN {col} DROP DEFAULT")
            } else {
                format!("ALTER COLUMN {col} SET DEFAULT {}", c.default_sql.trim())
            });
        }
        if old.comment != c.comment {
            comments.push((c.name.clone(), c.comment.clone()));
        }
    }
    actions.extend(tail);
    if !actions.is_empty() {
        out.push(format!(
            "ALTER TABLE {old_table}\n  {}",
            actions.join(",\n  ")
        ));
    }
    for index in new_indexes {
        out.push(postgres_index_sql(db, &before.name, index));
    }
    for (column, comment) in comments {
        out.push(format!(
            "COMMENT ON COLUMN {old_table}.{} IS {}",
            d.quote_ident(&column),
            if comment.is_empty() {
                "NULL".to_owned()
            } else {
                d.quote_str(&comment)
            }
        ));
    }
    if before.options.comment != after.options.comment {
        out.push(format!(
            "COMMENT ON TABLE {old_table} IS {}",
            if after.options.comment.is_empty() {
                "NULL".to_owned()
            } else {
                d.quote_str(&after.options.comment)
            }
        ));
    }
    if before.name != after.name {
        out.push(format!(
            "ALTER TABLE {old_table} RENAME TO {}",
            d.quote_ident(&after.name)
        ));
    }
    out.push("COMMIT".to_owned());
    out
}

/// What to run when applying `alter_sql`'s statements fails part-way, to leave the session
/// as it was.
pub fn recovery_statements(d: Dialect) -> &'static [&'static str] {
    match d {
        Dialect::MySql => &[],
        Dialect::Sqlite => &["ROLLBACK", "PRAGMA foreign_keys = ON"],
        Dialect::Postgres => &["ROLLBACK"],
    }
}

fn alter_mysql(d: Dialect, db: &str, before: &TableDesign, after: &TableDesign) -> Vec<String> {
    let mut clauses = Vec::new();

    // Foreign keys that went away or changed are dropped first, as they may need an index
    // that is dropped below; changed ones are re-added at the end.
    let mut renamed = before.clone();
    renamed.columns.clone_from(&after.columns);
    renamed.follow_renames();
    let mut added_fks = Vec::new();
    // Indexes dropped along with the foreign key they were made for.
    let mut replaced_indexes: Vec<String> = Vec::new();
    for (old, old_renamed) in before.foreign_keys.iter().zip(&renamed.foreign_keys) {
        let new = after
            .foreign_keys
            .iter()
            .find(|fk| fk.original_name.as_deref() == Some(old.name.as_str()));
        let unchanged = new.is_some_and(|new| {
            ForeignKeyDef {
                original_name: None,
                ..new.clone()
            } == ForeignKeyDef {
                original_name: None,
                ..old_renamed.clone()
            }
        });
        if !unchanged {
            clauses.push(format!("DROP FOREIGN KEY {}", d.quote_ident(&old.name)));
            if let Some(new) = new {
                added_fks.push(new);
                // The index the server made for the old key has the key's name; the new key
                // gets its own, so the old one goes (unless the user changed it themselves).
                let auto_index = before
                    .indexes
                    .iter()
                    .find(|i| i.name == old.name && i.columns == old.columns);
                let kept_as_is = auto_index.is_some_and(|i| {
                    after.indexes.iter().any(|a| {
                        a.original_name.as_deref() == Some(i.name.as_str())
                            && a.name == i.name
                            && a.kind == i.kind
                    })
                });
                if kept_as_is {
                    replaced_indexes.push(old.name.clone());
                }
            }
        }
    }
    added_fks.extend(
        after
            .foreign_keys
            .iter()
            .filter(|fk| fk.original_name.is_none()),
    );

    // Checks are compared as written: one naming a renamed column must be dropped and added
    // again with the new name, as MySQL won't rename a column a check uses.
    let mut added_checks = Vec::new();
    for old in &before.checks {
        let new = after
            .checks
            .iter()
            .find(|c| c.original_name.as_deref() == Some(old.name.as_str()));
        let unchanged =
            new.is_some_and(|new| new.name == old.name && new.clause.trim() == old.clause.trim());
        if !unchanged {
            clauses.push(format!("DROP CONSTRAINT {}", d.quote_ident(&old.name)));
            if let Some(new) = new {
                added_checks.push(new);
            }
        }
    }
    added_checks.extend(after.checks.iter().filter(|c| c.original_name.is_none()));

    // Indexes that went away or changed are dropped before any column, so dropping a column
    // can't remove an index this statement still refers to; changed ones are re-added last.
    let mut added_indexes = Vec::new();
    for name in &replaced_indexes {
        clauses.push(format!("DROP INDEX {}", d.quote_ident(name)));
    }
    for old in before
        .indexes
        .iter()
        .filter(|i| !replaced_indexes.contains(&i.name))
    {
        let new = after
            .indexes
            .iter()
            .find(|i| i.original_name.as_deref() == Some(old.name.as_str()));
        let mut renamed_old = before.clone();
        renamed_old.columns.clone_from(&after.columns);
        renamed_old.indexes = vec![old.clone()];
        renamed_old.follow_renames();
        let unchanged = new.is_some_and(|new| {
            new.name == old.name
                && new.kind == old.kind
                && new.columns == renamed_old.indexes[0].columns
        });
        if !unchanged {
            clauses.push(format!("DROP INDEX {}", d.quote_ident(&old.name)));
            if let Some(new) = new {
                added_indexes.push(new);
            }
        }
    }
    added_indexes.extend(after.indexes.iter().filter(|i| i.original_name.is_none()));

    // Dropped columns next, so a new column may reuse a dropped name.
    for old in &before.columns {
        let kept = after
            .columns
            .iter()
            .any(|c| c.original_name.as_deref() == Some(old.name.as_str()));
        if !kept {
            clauses.push(format!("DROP COLUMN {}", d.quote_ident(&old.name)));
        }
    }

    let pk_changed = before.primary_key() != after.primary_key();
    // A primary key column can't lose AUTO_INCREMENT's key while the key is being replaced, so
    // drop the old key before changing columns and add the new one after.
    if pk_changed && !before.primary_key().is_empty() {
        clauses.push("DROP PRIMARY KEY".into());
    }

    let old_order: Vec<&str> = before
        .columns
        .iter()
        .filter(|old| {
            after
                .columns
                .iter()
                .any(|c| c.original_name.as_deref() == Some(old.name.as_str()))
        })
        .map(|c| c.name.as_str())
        .collect();
    let new_order: Vec<&str> = after
        .columns
        .iter()
        .filter_map(|c| c.original_name.as_deref())
        .collect();

    for (ix, col) in after.columns.iter().enumerate() {
        let position = match ix {
            0 => " FIRST".to_owned(),
            _ => format!(" AFTER {}", d.quote_ident(&after.columns[ix - 1].name)),
        };
        match &col.original_name {
            None => clauses.push(format!("ADD COLUMN {}{position}", column_sql(d, col))),
            Some(original) => {
                let Some(old) = before.columns.iter().find(|c| &c.name == original) else {
                    continue;
                };
                // Compare everything but the key flag, which is handled separately.
                let changed = ColumnDef {
                    primary_key: false,
                    ..old.clone()
                } != ColumnDef {
                    primary_key: false,
                    original_name: old.original_name.clone(),
                    ..col.clone()
                };
                let moved = old_order != new_order;
                if changed || moved {
                    let position = if moved { position } else { String::new() };
                    clauses.push(format!(
                        "CHANGE COLUMN {} {}{position}",
                        d.quote_ident(original),
                        column_sql(d, col)
                    ));
                }
            }
        }
    }

    if pk_changed && !after.primary_key().is_empty() {
        let cols: Vec<String> = after
            .primary_key()
            .iter()
            .map(|c| d.quote_ident(c))
            .collect();
        clauses.push(format!("ADD PRIMARY KEY ({})", cols.join(", ")));
    }
    for index in added_indexes {
        clauses.push(format!("ADD {}", index_sql(d, index)));
    }
    let mut deferred = Vec::new();
    for fk in added_fks {
        let clause = format!("ADD {}", foreign_key_sql(d, db, fk));
        if before.foreign_keys.iter().any(|old| old.name == fk.name) {
            deferred.push(clause);
        } else {
            clauses.push(clause);
        }
    }
    for check in added_checks {
        clauses.push(format!("ADD {}", check_sql(d, check)));
    }
    let changed_options = TableOptions {
        engine: changed_text(&before.options.engine, &after.options.engine),
        collation: changed_text(&before.options.collation, &after.options.collation),
        ..TableOptions::default()
    };
    clauses.extend(options_sql(d, &changed_options));
    if before.options.comment != after.options.comment {
        clauses.push(format!("COMMENT={}", d.quote_str(&after.options.comment)));
    }
    if before.name != after.name {
        clauses.push(format!("RENAME TO {}", d.qualified(db, &after.name)));
    }
    let mut out = Vec::new();
    if !clauses.is_empty() {
        out.push(format!(
            "ALTER TABLE {}\n  {}",
            d.qualified(db, &before.name),
            clauses.join(",\n  ")
        ));
    }
    // Partitioning goes last and on its own: it can't be combined with most other changes.
    if partitioning_changed(&before.options.partitioning, &after.options.partitioning) {
        let partitioning = after.options.partitioning.trim();
        out.push(format!(
            "ALTER TABLE {}\n  {}",
            d.qualified(db, &after.name),
            if partitioning.is_empty() {
                "REMOVE PARTITIONING"
            } else {
                partitioning
            }
        ));
    }
    if !deferred.is_empty() {
        out.push(format!(
            "ALTER TABLE {}\n  {}",
            d.qualified(db, &after.name),
            deferred.join(",\n  ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Dialect = Dialect::MySql;

    fn col(name: &str, data_type: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
            original_name: Some(name.into()),
            ..ColumnDef::default()
        }
    }

    fn table() -> TableDesign {
        TableDesign {
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    primary_key: true,
                    auto_increment: true,
                    nullable: false,
                    ..col("id", "INT UNSIGNED")
                },
                ColumnDef {
                    default_sql: "'n/a'".into(),
                    comment: "who".into(),
                    ..col("name", "VARCHAR(40)")
                },
            ],
            indexes: Vec::new(),
            ..TableDesign::default()
        }
    }

    #[test]
    fn sqlite_creates_and_rebuilds() {
        let s = Dialect::Sqlite;
        let mut t = table();
        t.columns[0].data_type = "INTEGER".into();
        t.indexes.push(IndexDef {
            original_name: None,
            ..index("by_name", IndexKind::Unique, &["name"])
        });
        assert!(t.validate_for(s).is_ok());
        assert_eq!(
            create_statements(s, "main", &t),
            [
                "CREATE TABLE \"main\".\"t\" (\n  \"id\" INTEGER PRIMARY KEY AUTOINCREMENT \
                 NOT NULL,\n  \"name\" VARCHAR(40) DEFAULT 'n/a'\n)",
                "CREATE UNIQUE INDEX \"main\".\"by_name\" ON \"t\" (\"name\")",
            ]
        );
        let mut loaded = t.clone();
        loaded.indexes[0].original_name = Some("by_name".into());
        let before = loaded.with_sqlite_parts(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT) STRICT;",
            vec!["CREATE TRIGGER x AFTER INSERT ON t BEGIN SELECT 1; END".into()],
        );
        assert_eq!(before.suffix, "STRICT");
        assert_eq!(alter_sql(s, "main", &before, &before).len(), 0);
        let mut renamed = before.clone();
        renamed.name = "u".into();
        assert_eq!(
            alter_sql(s, "main", &before, &renamed),
            ["ALTER TABLE \"main\".\"t\" RENAME TO \"u\""]
        );
        let mut after = before.clone();
        after.columns[1].name = "label".into();
        after.columns.push(ColumnDef {
            original_name: None,
            ..col("born", "TEXT")
        });
        let sql = alter_sql(s, "main", &before, &after);
        assert_eq!(sql[0], "PRAGMA foreign_keys = OFF");
        assert!(sql[2].starts_with("CREATE TABLE \"main\".\"ovs_rebuild_t\""));
        assert!(sql[2].ends_with(") STRICT"), "{}", sql[2]);
        assert_eq!(
            sql[3],
            "INSERT INTO \"main\".\"ovs_rebuild_t\" (\"id\", \"label\") SELECT \"id\", \"name\" FROM \"main\".\"t\""
        );
        assert!(sql.contains(&"CREATE TRIGGER x AFTER INSERT ON t BEGIN SELECT 1; END".to_owned()));
        assert_eq!(sql.last().unwrap(), "PRAGMA foreign_keys = ON");
        t.indexes[0].kind = IndexKind::Fulltext;
        assert!(t.validate_for(s).is_err());
    }

    #[test]
    fn checks_follow_renames() {
        let mut before = table().with_checks(&[CheckInfo {
            name: "name_ok".into(),
            clause: "(`name` <> 'x')".into(),
        }]);
        before.checks.push(CheckDef {
            name: "id_ok".into(),
            clause: "`id` > 0".into(),
            original_name: Some("id_ok".into()),
        });
        assert!(create_sql(D, "s", &before).ends_with(
            "  CONSTRAINT `name_ok` CHECK ((`name` <> 'x')),\n  CONSTRAINT `id_ok` CHECK (`id` > 0)\n)"
        ));
        let mut after = before.clone();
        after.columns[1].name = "label".into();
        after.follow_renames();
        assert_eq!(after.checks[0].clause, "(`label` <> 'x')");
        let sql = alter_sql(D, "s", &before, &after).join(";\n");
        assert!(
            sql.starts_with("ALTER TABLE `s`.`t`\n  DROP CONSTRAINT `name_ok`,"),
            "{sql}"
        );
        assert!(
            sql.ends_with("ADD CONSTRAINT `name_ok` CHECK ((`label` <> 'x'))"),
            "{sql}"
        );
        assert!(!sql.contains("id_ok"), "{sql}");
        after.checks[1].clause = " ".into();
        assert!(after.validate().unwrap_err().contains("expression"));
    }

    #[test]
    fn partitioning() {
        let mut t = table();
        t.options.partitioning = "PARTITION BY HASH (`id`) PARTITIONS 4".into();
        t.validate_for(D).unwrap();
        assert!(
            create_sql(D, "s", &t).ends_with(")\nPARTITION BY HASH (`id`) PARTITIONS 4"),
            "{}",
            create_sql(D, "s", &t)
        );

        let before = t.clone();
        let mut after = t.clone();
        // Spacing alone isn't a change.
        after.options.partitioning = "PARTITION BY  HASH (`id`)\n  PARTITIONS 4".into();
        assert_eq!(alter_sql(D, "s", &before, &after).len(), 0);
        after.options.partitioning = "PARTITION BY KEY (`id`) PARTITIONS 2".into();
        after.columns[1].name = "label".into();
        let sql = alter_sql(D, "s", &before, &after);
        assert_eq!(sql.len(), 2, "{sql:?}");
        assert_eq!(
            sql[1],
            "ALTER TABLE `s`.`t`\n  PARTITION BY KEY (`id`) PARTITIONS 2"
        );
        after.options.partitioning = String::new();
        assert!(
            alter_sql(D, "s", &before, &after)[1].ends_with("REMOVE PARTITIONING"),
            "{:?}",
            alter_sql(D, "s", &before, &after)
        );

        after.options.partitioning = "HASH (`id`)".into();
        assert!(after.validate_for(D).unwrap_err().contains("PARTITION BY"));
        after.options.partitioning = "partition by hash (id)".into();
        after.foreign_keys.push(ForeignKeyDef {
            name: "fk".into(),
            columns: vec!["id".into()],
            ref_table: "o".into(),
            ref_columns: vec!["id".into()],
            ..ForeignKeyDef::default()
        });
        assert!(after.validate_for(D).unwrap_err().contains("foreign keys"));
    }

    #[test]
    fn foreign_keys_and_options() {
        let mut t = table();
        t.options = TableOptions {
            engine: "InnoDB".into(),
            collation: "utf8mb4_unicode_ci".into(),
            comment: "people's".into(),
            ..TableOptions::default()
        };
        t.foreign_keys.push(ForeignKeyDef {
            name: "fk_owner".into(),
            columns: vec!["name".into()],
            ref_table: "owners".into(),
            ref_columns: vec!["login".into()],
            on_delete: "cascade".into(),
            ..ForeignKeyDef::default()
        });
        assert!(t.validate().is_ok());
        assert!(create_sql(D, "s", &t).ends_with(
            "  CONSTRAINT `fk_owner` FOREIGN KEY (`name`) REFERENCES `s`.`owners` (`login`) \
             ON DELETE CASCADE\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 \
             COLLATE=utf8mb4_unicode_ci COMMENT='people\\'s'"
        ));

        let mut before = t.clone();
        before.foreign_keys[0].original_name = Some("fk_owner".into());
        let mut after = before.clone();
        after.columns[1].name = "owner".into();
        after.follow_renames();
        after.options.engine = "innodb".into();
        // A rename alone keeps the constraint; only the column changes.
        let sql = alter_sql(D, "s", &before, &after).join(";\n");
        assert!(!sql.contains("FOREIGN KEY"), "{sql}");
        assert!(!sql.contains("ENGINE"), "{sql}");

        after.foreign_keys[0].on_update = "SET NULL".into();
        after.options.comment = String::new();
        let sql = alter_sql(D, "s", &before, &after).join(";\n");
        assert!(sql.contains("  DROP FOREIGN KEY `fk_owner`,"), "{sql}");
        assert!(
            sql.contains("ADD CONSTRAINT `fk_owner` FOREIGN KEY (`owner`)"),
            "{sql}"
        );
        // Re-created under its old name, in a statement of its own.
        assert!(sql.ends_with("COMMENT='';\nALTER TABLE `s`.`t`\n  ADD CONSTRAINT `fk_owner` FOREIGN KEY (`owner`) REFERENCES `s`.`owners` (`login`) ON DELETE CASCADE ON UPDATE SET NULL"), "{sql}");

        after.foreign_keys[0].ref_columns.push("x".into());
        assert!(after.validate().unwrap_err().contains("as many"));
    }

    fn index(name: &str, kind: IndexKind, columns: &[&str]) -> IndexDef {
        IndexDef {
            name: name.into(),
            kind,
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            original_name: Some(name.into()),
        }
    }

    #[test]
    fn creates_and_alters_indexes() {
        let mut t = table();
        t.indexes.push(IndexDef {
            original_name: None,
            ..index("by_name", IndexKind::Unique, &["name(10)", "id"])
        });
        assert!(
            create_sql(D, "s", &t)
                .ends_with("PRIMARY KEY (`id`),\n  UNIQUE INDEX `by_name` (`name`(10), `id`)\n)")
        );

        let mut before = table();
        before.indexes = vec![
            index("keep", IndexKind::Index, &["name"]),
            index("gone", IndexKind::Index, &["id", "name"]),
            index("widen", IndexKind::Index, &["name"]),
        ];
        let mut after = before.clone();
        // Renaming a column carries the index along without touching it.
        after.columns[1].name = "label".into();
        after.follow_renames();
        assert_eq!(after.indexes[0].columns, ["label"]);
        after.indexes.remove(1);
        after.indexes[1].columns.push("id".into());
        after.indexes.push(IndexDef {
            original_name: None,
            ..index("ft", IndexKind::Fulltext, &["label"])
        });
        let sql = alter_sql(D, "s", &before, &after).join(";\n");
        assert!(!sql.contains("`keep`"), "{sql}");
        assert!(
            sql.starts_with("ALTER TABLE `s`.`t`\n  DROP INDEX `gone`,\n  DROP INDEX `widen`,"),
            "{sql}"
        );
        assert!(
            sql.ends_with(
                "ADD INDEX `widen` (`label`, `id`),\n  ADD FULLTEXT INDEX `ft` (`label`)"
            ),
            "{sql}"
        );
        assert!(after.validate().is_ok());
        after.indexes[0].columns = vec!["nope".into()];
        assert!(after.validate().unwrap_err().contains("unknown column"));
    }

    #[test]
    fn creates_tables() {
        assert_eq!(
            create_sql(D, "shop", &table()),
            "CREATE TABLE `shop`.`t` (\n  `id` INT UNSIGNED NOT NULL AUTO_INCREMENT,\n  \
             `name` VARCHAR(40) NULL DEFAULT 'n/a' COMMENT 'who',\n  PRIMARY KEY (`id`)\n)"
        );
    }

    #[test]
    fn unchanged_design_needs_no_alter() {
        assert_eq!(alter_sql(D, "shop", &table(), &table()).len(), 0);
    }

    #[test]
    fn alters_rename_modify_add_drop() {
        let before = table();
        let mut after = table();
        after.columns[1].name = "full_name".into();
        after.columns[1].data_type = "VARCHAR(100)".into();
        after.columns.push(ColumnDef {
            original_name: None,
            ..col("born", "DATE")
        });
        assert_eq!(
            alter_sql(D, "shop", &before, &after).join(";\n"),
            "ALTER TABLE `shop`.`t`\n  CHANGE COLUMN `name` `full_name` VARCHAR(100) NULL \
             DEFAULT 'n/a' COMMENT 'who',\n  ADD COLUMN `born` DATE NULL AFTER `full_name`"
        );
        let mut dropped = table();
        dropped.columns.pop();
        assert_eq!(
            alter_sql(D, "shop", &before, &dropped).join(";\n"),
            "ALTER TABLE `shop`.`t`\n  DROP COLUMN `name`"
        );
    }

    #[test]
    fn reorders_and_changes_primary_key() {
        let before = table();
        let mut after = table();
        after.columns.swap(0, 1);
        after.columns[0].primary_key = true;
        after.columns[0].nullable = false;
        let sql = alter_sql(D, "shop", &before, &after).join(";\n");
        assert!(sql.contains("DROP PRIMARY KEY"), "{sql}");
        assert!(
            sql.contains("CHANGE COLUMN `name` `name` VARCHAR(40) NOT NULL"),
            "{sql}"
        );
        assert!(sql.contains(" FIRST"), "{sql}");
        assert!(sql.ends_with("ADD PRIMARY KEY (`name`, `id`)"), "{sql}");
    }

    #[test]
    fn loads_existing_tables() {
        let columns = [
            ColumnInfo {
                name: "id".into(),
                column_type: "int(10) unsigned".into(),
                key: "PRI".into(),
                extra: "auto_increment".into(),
                ..ColumnInfo::default()
            },
            ColumnInfo {
                name: "status".into(),
                column_type: "varchar(10)".into(),
                nullable: true,
                default: Some("new".into()),
                ..ColumnInfo::default()
            },
            ColumnInfo {
                name: "at".into(),
                column_type: "datetime".into(),
                default: Some("CURRENT_TIMESTAMP".into()),
                ..ColumnInfo::default()
            },
        ];
        let indexes = [
            IndexInfo {
                name: "PRIMARY".into(),
                columns: vec!["id".into()],
                unique: true,
                index_type: "BTREE".into(),
                prefix_lengths: Vec::new(),
                constraint_name: String::new(),
            },
            IndexInfo {
                name: "by_status".into(),
                columns: vec!["status".into(), "at".into()],
                unique: true,
                index_type: "BTREE".into(),
                prefix_lengths: vec![Some(4), None],
                constraint_name: String::new(),
            },
        ];
        let design = TableDesign::from_table(D, "t", &columns, &indexes);
        assert!(design.columns[0].primary_key && design.columns[0].auto_increment);
        assert_eq!(design.columns[1].default_sql, "'new'");
        assert_eq!(design.columns[2].default_sql, "CURRENT_TIMESTAMP");
        assert_eq!(
            design.indexes,
            [index("by_status", IndexKind::Unique, &["status(4)", "at"])]
        );
        assert!(design.validate().is_ok());
    }

    #[test]
    fn validation_catches_mistakes() {
        let mut t = table();
        t.columns[1].name = "ID".into();
        assert!(t.validate().unwrap_err().contains("twice"));
        t.columns[1].name = String::new();
        assert!(t.validate().is_err());
    }
}
