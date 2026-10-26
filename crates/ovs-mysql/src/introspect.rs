//! Schema queries behind the object browser.

use ovs_core::sql::Dialect;
use ovs_core::{
    CheckInfo, ColumnInfo, Error, ForeignKeyInfo, IndexInfo, ObjectKind, Result, ResultSet,
    SchemaObject, TableOptions, Value,
};

use crate::MySqlSession;

const D: Dialect = Dialect::MySql;

/// Looks up a cell by column name (case-insensitive), so we don't depend on column order, which
/// differs between MySQL and MariaDB versions.
fn cell<'a>(rs: &'a ResultSet, row: &'a [Value], name: &str) -> Option<&'a Value> {
    let idx = rs
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))?;
    row.get(idx)
}

fn text(rs: &ResultSet, row: &[Value], name: &str) -> String {
    match cell(rs, row, name) {
        None | Some(Value::Null) => String::new(),
        Some(v) => v.to_string(),
    }
}

pub(crate) async fn databases(s: &mut MySqlSession) -> Result<Vec<String>> {
    let rs = s.query_set("SHOW DATABASES").await?;
    Ok(rs.rows.iter().map(|r| r[0].to_string()).collect())
}

pub(crate) async fn objects(
    s: &mut MySqlSession,
    db: &str,
    kind: ObjectKind,
) -> Result<Vec<SchemaObject>> {
    let schema = D.quote_str(db);
    let sql = match kind {
        ObjectKind::Database => {
            let names = databases(s).await?;
            return Ok(names
                .into_iter()
                .map(|name| SchemaObject {
                    name,
                    kind,
                    detail: String::new(),
                })
                .collect());
        }
        ObjectKind::Table => format!(
            "SELECT TABLE_NAME AS name, TABLE_COMMENT AS detail FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = {schema} AND TABLE_TYPE <> 'VIEW' ORDER BY TABLE_NAME"
        ),
        ObjectKind::View => format!(
            "SELECT TABLE_NAME AS name, '' AS detail FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = {schema} AND TABLE_TYPE = 'VIEW' ORDER BY TABLE_NAME"
        ),
        ObjectKind::Procedure | ObjectKind::Function => format!(
            "SELECT ROUTINE_NAME AS name, ROUTINE_COMMENT AS detail \
             FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = {schema} \
             AND ROUTINE_TYPE = '{}' ORDER BY ROUTINE_NAME",
            kind.sql_keyword()
        ),
        ObjectKind::Trigger => format!(
            "SELECT TRIGGER_NAME AS name, EVENT_OBJECT_TABLE AS detail \
             FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = {schema} \
             ORDER BY TRIGGER_NAME"
        ),
        ObjectKind::Event => format!(
            "SELECT EVENT_NAME AS name, STATUS AS detail FROM information_schema.EVENTS \
             WHERE EVENT_SCHEMA = {schema} ORDER BY EVENT_NAME"
        ),
    };
    let rs = s.query_set(&sql).await?;
    Ok(rs
        .rows
        .iter()
        .map(|r| SchemaObject {
            name: text(&rs, r, "name"),
            kind,
            detail: text(&rs, r, "detail"),
        })
        .collect())
}

pub(crate) async fn columns(
    s: &mut MySqlSession,
    db: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>> {
    let rs = s
        .query_set(&format!(
            "SHOW FULL COLUMNS FROM {}",
            D.qualified(db, table)
        ))
        .await?;
    Ok(rs
        .rows
        .iter()
        .map(|r| ColumnInfo {
            name: text(&rs, r, "Field"),
            column_type: text(&rs, r, "Type"),
            nullable: text(&rs, r, "Null") == "YES",
            key: text(&rs, r, "Key"),
            default: match cell(&rs, r, "Default") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.to_string()),
            },
            extra: text(&rs, r, "Extra"),
            comment: text(&rs, r, "Comment"),
            collation: text(&rs, r, "Collation"),
        })
        .collect())
}

pub(crate) async fn indexes(s: &mut MySqlSession, db: &str, table: &str) -> Result<Vec<IndexInfo>> {
    let rs = s
        .query_set(&format!("SHOW INDEX FROM {}", D.qualified(db, table)))
        .await?;
    // Rows come one per (index, column) in Seq_in_index order; fold them into indexes.
    let mut out: Vec<IndexInfo> = Vec::new();
    for r in &rs.rows {
        let name = text(&rs, r, "Key_name");
        let column = text(&rs, r, "Column_name");
        let prefix = text(&rs, r, "Sub_part").parse::<u32>().ok();
        let idx = if let Some(idx) = out.iter_mut().find(|i| i.name == name) {
            idx.columns.push(column);
            idx
        } else {
            out.push(IndexInfo {
                unique: text(&rs, r, "Non_unique") == "0",
                index_type: text(&rs, r, "Index_type"),
                columns: vec![column],
                name,
                prefix_lengths: Vec::new(),
            });
            out.last_mut().expect("just pushed")
        };
        if prefix.is_some() || !idx.prefix_lengths.is_empty() {
            idx.prefix_lengths.resize(idx.columns.len() - 1, None);
            idx.prefix_lengths.push(prefix);
        }
    }
    Ok(out)
}

pub(crate) async fn foreign_keys(
    s: &mut MySqlSession,
    db: &str,
    table: &str,
) -> Result<Vec<ForeignKeyInfo>> {
    let sql = format!(
        "SELECT k.CONSTRAINT_NAME AS name, k.COLUMN_NAME AS col, \
         k.REFERENCED_TABLE_SCHEMA AS ref_db, k.REFERENCED_TABLE_NAME AS ref_table, \
         k.REFERENCED_COLUMN_NAME AS ref_col, r.UPDATE_RULE AS on_update, \
         r.DELETE_RULE AS on_delete \
         FROM information_schema.KEY_COLUMN_USAGE k \
         JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
           ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
          AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND r.TABLE_NAME = k.TABLE_NAME \
         WHERE k.TABLE_SCHEMA = {} AND k.TABLE_NAME = {} \
           AND k.REFERENCED_TABLE_NAME IS NOT NULL \
         ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
        D.quote_str(db),
        D.quote_str(table)
    );
    let rs = s.query_set(&sql).await?;
    let mut out: Vec<ForeignKeyInfo> = Vec::new();
    for r in &rs.rows {
        let name = text(&rs, r, "name");
        if out.last().is_none_or(|fk| fk.name != name) {
            out.push(ForeignKeyInfo {
                name,
                ref_database: text(&rs, r, "ref_db"),
                ref_table: text(&rs, r, "ref_table"),
                on_update: text(&rs, r, "on_update"),
                on_delete: text(&rs, r, "on_delete"),
                ..ForeignKeyInfo::default()
            });
        }
        let fk = out.last_mut().expect("pushed above");
        fk.columns.push(text(&rs, r, "col"));
        fk.ref_columns.push(text(&rs, r, "ref_col"));
    }
    Ok(out)
}

pub(crate) async fn checks(s: &mut MySqlSession, db: &str, table: &str) -> Result<Vec<CheckInfo>> {
    // information_schema escapes quotes in CHECK_CLAUSE (`\'`), which isn't valid SQL, so the
    // expressions come from SHOW CREATE TABLE. Column-level checks (MariaDB, e.g. JSON's
    // json_valid) are part of their column and aren't listed.
    let ddl = ddl(s, db, table, ObjectKind::Table).await?;
    Ok(parse_checks(&ddl))
}

/// The `CONSTRAINT name CHECK (expr)` lines of a `SHOW CREATE TABLE` statement.
fn parse_checks(ddl: &str) -> Vec<CheckInfo> {
    ddl.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("CONSTRAINT `")?;
            // The name ends at the first backtick that isn't doubled.
            let mut name = String::new();
            let mut chars = rest.char_indices().peekable();
            let end = loop {
                let (i, c) = chars.next()?;
                if c == '`' {
                    if chars.peek().is_some_and(|(_, n)| *n == '`') {
                        chars.next();
                    } else {
                        break i + 1;
                    }
                }
                name.push(c);
            };
            let body = rest[end..].trim_start().strip_prefix("CHECK (")?;
            let body = body.trim_end().trim_end_matches(',');
            // MySQL marks unenforced checks with a version comment after the expression.
            let body = body.rfind(" /*!").map_or(body, |at| &body[..at]).trim_end();
            let clause = body.strip_suffix(')')?;
            Some(CheckInfo {
                name,
                clause: clause.to_owned(),
            })
        })
        .collect()
}

pub(crate) async fn table_options(
    s: &mut MySqlSession,
    db: &str,
    table: &str,
) -> Result<TableOptions> {
    let sql = format!(
        "SELECT ENGINE AS engine, TABLE_COLLATION AS collation, TABLE_COMMENT AS comment \
         FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
        D.quote_str(db),
        D.quote_str(table)
    );
    let rs = s.query_set(&sql).await?;
    let row = rs
        .rows
        .first()
        .ok_or_else(|| Error::Other(format!("table {table} not found")))?;
    Ok(TableOptions {
        engine: text(&rs, row, "engine"),
        collation: text(&rs, row, "collation"),
        comment: text(&rs, row, "comment"),
    })
}

pub(crate) async fn ddl(
    s: &mut MySqlSession,
    db: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<String> {
    let target = if kind == ObjectKind::Database {
        D.quote_ident(name)
    } else {
        D.qualified(db, name)
    };
    let rs = s
        .query_set(&format!("SHOW CREATE {} {target}", kind.sql_keyword()))
        .await?;
    let row = rs
        .rows
        .first()
        .ok_or_else(|| Error::Other(format!("{name} not found")))?;
    // The DDL column is "Create Table", "Create View", "Create Procedure", ...; triggers call it
    // "SQL Original Statement".
    rs.columns
        .iter()
        .position(|c| c.name.starts_with("Create ") || c.name == "SQL Original Statement")
        .and_then(|i| row.get(i))
        .map_or_else(
            || Err(Error::Other("unexpected SHOW CREATE result".into())),
            |v| match v {
                Value::Null => Err(Error::Unsupported(format!(
                    "no privilege to view the definition of {name}"
                ))),
                v => Ok(v.to_string()),
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checks_from_create_table() {
        let ddl = "CREATE TABLE `t` (\n  `a` int(11) DEFAULT NULL CHECK (`a` > 0),\n  \
                   CONSTRAINT `b ``ok` CHECK ((`b` <> _utf8mb4'x\\'y')) /*!80016 NOT ENFORCED */,\n  \
                   CONSTRAINT `t_chk_1` CHECK (`a` > 0),\n  CONSTRAINT `fk` FOREIGN KEY (`a`) REFERENCES `o` (`id`)\n) ENGINE=InnoDB";
        let checks = parse_checks(ddl);
        assert_eq!(checks.len(), 2, "{checks:?}");
        assert_eq!(checks[0].name, "b `ok");
        assert_eq!(checks[0].clause, "(`b` <> _utf8mb4'x\\'y')");
        assert_eq!(checks[1].clause, "`a` > 0");
    }
}
