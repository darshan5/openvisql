//! Schema queries behind the object browser.

use ovs_core::sql::Dialect;
use ovs_core::{ColumnInfo, Error, IndexInfo, ObjectKind, Result, ResultSet, SchemaObject, Value};

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
        if let Some(idx) = out.iter_mut().find(|i| i.name == name) {
            idx.columns.push(column);
        } else {
            out.push(IndexInfo {
                unique: text(&rs, r, "Non_unique") == "0",
                index_type: text(&rs, r, "Index_type"),
                columns: vec![column],
                name,
            });
        }
    }
    Ok(out)
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
