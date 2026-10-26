//! SQLite support, built on `rusqlite` with SQLite compiled in.
//!
//! SQLite is synchronous, so every call runs on tokio's blocking pool. Each session opens the
//! database file on its own, like a connection to a server would.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ovs_core::sql::Dialect;
use ovs_core::{
    CancelHandle, CheckInfo, Column, ColumnInfo, ConnectionConfig, Driver, Engine, Error,
    ForeignKeyInfo, IndexInfo, ObjectKind, QueryOutcome, Result, ResultSet, SchemaObject, Session,
    TableOptions, Value,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, InterruptHandle, OpenFlags};

const D: Dialect = Dialect::Sqlite;

#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteDriver;

#[async_trait]
impl Driver for SqliteDriver {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        _password: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        let path = config.path.trim().to_owned();
        if path.is_empty() {
            return Err(Error::Connection("choose a database file".into()));
        }
        let init = config.init_commands.clone();
        let timeout = Duration::from_secs(config.connect_timeout_secs.max(1));
        let conn = blocking(move || {
            let conn = Connection::open_with_flags(&path, OpenFlags::default()).map_err(map_err)?;
            // Wait for other sessions' write locks instead of failing at once.
            conn.busy_timeout(timeout).map_err(map_err)?;
            conn.execute_batch("PRAGMA foreign_keys = ON")
                .map_err(map_err)?;
            if !init.trim().is_empty() {
                conn.execute_batch(&init).map_err(map_err)?;
            }
            Ok(conn)
        })
        .await?;
        let interrupt = conn.get_interrupt_handle();
        Ok(Box::new(SqliteSession {
            conn: Arc::new(Mutex::new(conn)),
            cancel: Arc::new(SqliteCancel(interrupt)),
            version: format!("SQLite {}", rusqlite::version()),
            current: "main".to_owned(),
        }))
    }
}

/// Runs blocking SQLite work off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Other(format!("SQLite worker failed: {e}")))?
}

fn map_err(err: rusqlite::Error) -> Error {
    match err {
        rusqlite::Error::SqliteFailure(e, _) if e.code == ErrorCode::OperationInterrupted => {
            Error::Cancelled
        }
        rusqlite::Error::SqliteFailure(e, message) => Error::server(
            u32::try_from(e.extended_code).ok(),
            message.unwrap_or_else(|| e.to_string()),
        ),
        rusqlite::Error::SqlInputError { error, msg, .. } => {
            Error::server(u32::try_from(error.extended_code).ok(), msg)
        }
        other => Error::Other(other.to_string()),
    }
}

struct SqliteCancel(InterruptHandle);

#[async_trait]
impl CancelHandle for SqliteCancel {
    async fn cancel(&self) -> Result<()> {
        self.0.interrupt();
        Ok(())
    }
}

pub struct SqliteSession {
    conn: Arc<Mutex<Connection>>,
    cancel: Arc<SqliteCancel>,
    version: String,
    /// The schema `USE` picked (`main` or an attached database). SQLite has no `USE`, so it
    /// only decides what the object browser and editor treat as current.
    current: String,
}

impl SqliteSession {
    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let conn = self.conn.clone();
        blocking(move || {
            let conn = conn
                .lock()
                .map_err(|_| Error::Other("SQLite connection poisoned".into()))?;
            f(&conn)
        })
        .await
    }

    /// Runs an internal query and returns all its rows.
    async fn rows(&self, sql: String) -> Result<ResultSet> {
        self.with(move |c| match run(c, &sql, None)?.into_iter().next() {
            Some(QueryOutcome::Rows(rs)) => Ok(rs),
            _ => Ok(ResultSet::default()),
        })
        .await
    }
}

fn convert(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
    }
}

/// Runs `sql`, one statement or several, stopping at the first error.
fn run(c: &Connection, sql: &str, row_limit: Option<usize>) -> Result<Vec<QueryOutcome>> {
    let statements = ovs_core::sql::split_statements(sql);
    if statements.len() <= 1 {
        return run_one(c, sql, row_limit);
    }
    let mut out = Vec::new();
    for statement in statements {
        out.extend(run_one(c, &statement.sql, row_limit)?);
    }
    Ok(out)
}

fn run_one(c: &Connection, sql: &str, row_limit: Option<usize>) -> Result<Vec<QueryOutcome>> {
    let sql = sql.trim().trim_end_matches(';').trim_end();
    let mut stmt = c.prepare(sql).map_err(map_err)?;
    if stmt.column_count() == 0 {
        stmt.raw_execute().map_err(map_err)?;
        return Ok(vec![affected(c, sql)]);
    }
    let decl: Vec<Option<String>> = stmt
        .columns()
        .map(|col| col.decl_type().map(str::to_owned))
        .collect();
    let columns: Vec<Column> = (0..stmt.column_count())
        .map(|i| {
            let name = stmt.column_name(i).map(str::to_owned).unwrap_or_default();
            let type_name = decl[i].clone().unwrap_or_default().to_ascii_uppercase();
            let mut column = Column {
                binary: type_name.contains("BLOB"),
                name,
                type_name,
                nullable: true,
                ..Column::default()
            };
            if let Ok(Some((db, table, origin, _, _, not_null, primary_key, _))) =
                stmt.column_metadata(i)
            {
                column.schema = db.to_string_lossy().into_owned();
                column.table = table.to_string_lossy().into_owned();
                column.origin_name = origin.to_string_lossy().into_owned();
                column.nullable = !not_null;
                column.primary_key = primary_key;
            }
            column
        })
        .collect();
    let width = columns.len();
    let mut set = ResultSet {
        columns,
        rows: Vec::new(),
        truncated: false,
    };
    let mut rows = stmt.raw_query();
    while let Some(row) = rows.next().map_err(map_err)? {
        if row_limit.is_some_and(|limit| set.rows.len() >= limit) {
            set.truncated = true;
            break;
        }
        let mut values = Vec::with_capacity(width);
        for i in 0..width {
            values.push(convert(row.get_ref(i).map_err(map_err)?));
        }
        set.rows.push(values);
    }
    Ok(vec![QueryOutcome::Rows(set)])
}

fn affected(c: &Connection, sql: &str) -> QueryOutcome {
    let first = sql
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let inserted = matches!(first.as_str(), "INSERT" | "REPLACE");
    QueryOutcome::Affected {
        rows: c.changes(),
        last_insert_id: if inserted {
            u64::try_from(c.last_insert_rowid())
                .ok()
                .filter(|id| *id > 0)
        } else {
            None
        },
        warnings: 0,
        info: String::new(),
    }
}

fn text(rs: &ResultSet, row: &[Value], name: &str) -> String {
    let Some(i) = rs
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))
    else {
        return String::new();
    };
    match row.get(i) {
        None | Some(Value::Null) => String::new(),
        Some(v) => v.to_string(),
    }
}

fn int(rs: &ResultSet, row: &[Value], name: &str) -> i64 {
    text(rs, row, name).parse().unwrap_or_default()
}

/// A default as SQLite stores it (an SQL expression) as the plain value other engines report:
/// `'it''s'` becomes `it's`; numbers and expressions are left alone.
fn unquote_default(sql: &str) -> String {
    match sql.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        Some(inner) => inner.replace("''", "'"),
        None => sql.to_owned(),
    }
}

#[async_trait]
impl Session for SqliteSession {
    fn dialect(&self) -> Dialect {
        D
    }

    fn server_version(&self) -> &str {
        &self.version
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    async fn execute(&mut self, sql: &str, row_limit: Option<usize>) -> Result<Vec<QueryOutcome>> {
        let sql = sql.to_owned();
        self.with(move |c| run(c, &sql, row_limit)).await
    }

    async fn ping(&mut self) -> Result<()> {
        self.rows("SELECT 1".into()).await.map(|_| ())
    }

    async fn current_database(&mut self) -> Result<Option<String>> {
        Ok(Some(self.current.clone()))
    }

    async fn use_database(&mut self, database: &str) -> Result<()> {
        if !self.list_databases().await?.iter().any(|d| d == database) {
            return Err(Error::server(None, format!("unknown database {database}")));
        }
        database.clone_into(&mut self.current);
        Ok(())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rs = self.rows("PRAGMA database_list".into()).await?;
        Ok(rs
            .rows
            .iter()
            .map(|r| text(&rs, r, "name"))
            // The temp schema exists on every connection and is private to it.
            .filter(|name| name != "temp")
            .collect())
    }

    async fn list_objects(
        &mut self,
        database: &str,
        kind: ObjectKind,
    ) -> Result<Vec<SchemaObject>> {
        let kind_name = match kind {
            ObjectKind::Database => {
                return Ok(self
                    .list_databases()
                    .await?
                    .into_iter()
                    .map(|name| SchemaObject {
                        name,
                        kind,
                        detail: String::new(),
                    })
                    .collect());
            }
            ObjectKind::Table => "table",
            ObjectKind::View => "view",
            ObjectKind::Trigger => "trigger",
            ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Event => {
                return Ok(Vec::new());
            }
        };
        let rs = self
            .rows(format!(
                "SELECT name, tbl_name FROM {}.sqlite_master WHERE type = '{kind_name}' \
                 AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
                D.quote_ident(database)
            ))
            .await?;
        Ok(rs
            .rows
            .iter()
            .map(|r| SchemaObject {
                name: text(&rs, r, "name"),
                kind,
                detail: if kind == ObjectKind::Trigger {
                    text(&rs, r, "tbl_name")
                } else {
                    String::new()
                },
            })
            .collect())
    }

    async fn list_columns(&mut self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let rs = self
            .rows(format!(
                "PRAGMA {}.table_xinfo({})",
                D.quote_ident(database),
                D.quote_str(table)
            ))
            .await?;
        let pk_count = rs.rows.iter().filter(|r| int(&rs, r, "pk") > 0).count();
        Ok(rs
            .rows
            .iter()
            // Hidden columns of virtual tables (1) aren't real columns.
            .filter(|r| int(&rs, r, "hidden") != 1)
            .map(|r| {
                let column_type = text(&rs, r, "type");
                let primary = int(&rs, r, "pk") > 0;
                // A lone INTEGER PRIMARY KEY is the rowid, which numbers new rows itself.
                let rowid = primary && pk_count == 1 && column_type.eq_ignore_ascii_case("INTEGER");
                let extra = match int(&rs, r, "hidden") {
                    2 => "VIRTUAL GENERATED",
                    3 => "STORED GENERATED",
                    _ if rowid => "auto_increment",
                    _ => "",
                };
                let default = match rs
                    .columns
                    .iter()
                    .position(|c| c.name == "dflt_value")
                    .and_then(|i| r.get(i))
                {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(unquote_default(&v.to_string())),
                };
                ColumnInfo {
                    name: text(&rs, r, "name"),
                    column_type,
                    nullable: int(&rs, r, "notnull") == 0 && !rowid,
                    key: if primary { "PRI".into() } else { String::new() },
                    default,
                    extra: extra.to_owned(),
                    comment: String::new(),
                }
            })
            .collect())
    }

    async fn list_indexes(&mut self, database: &str, table: &str) -> Result<Vec<IndexInfo>> {
        let schema = D.quote_ident(database);
        // The primary key, in key order, whether or not SQLite made an index for it.
        let info = self
            .rows(format!(
                "PRAGMA {schema}.table_info({})",
                D.quote_str(table)
            ))
            .await?;
        let mut key: Vec<(i64, String)> = info
            .rows
            .iter()
            .filter(|r| int(&info, r, "pk") > 0)
            .map(|r| (int(&info, r, "pk"), text(&info, r, "name")))
            .collect();
        key.sort();
        let mut out = Vec::new();
        if !key.is_empty() {
            out.push(IndexInfo {
                name: "PRIMARY".into(),
                columns: key.into_iter().map(|(_, name)| name).collect(),
                unique: true,
                index_type: "BTREE".into(),
                prefix_lengths: Vec::new(),
            });
        }
        let list = self
            .rows(format!(
                "PRAGMA {schema}.index_list({})",
                D.quote_str(table)
            ))
            .await?;
        for r in &list.rows {
            if text(&list, r, "origin") == "pk" {
                continue;
            }
            let name = text(&list, r, "name");
            let cols = self
                .rows(format!(
                    "PRAGMA {schema}.index_info({})",
                    D.quote_str(&name)
                ))
                .await?;
            out.push(IndexInfo {
                columns: cols.rows.iter().map(|c| text(&cols, c, "name")).collect(),
                unique: int(&list, r, "unique") == 1,
                index_type: "BTREE".into(),
                prefix_lengths: Vec::new(),
                name,
            });
        }
        Ok(out)
    }

    async fn list_foreign_keys(
        &mut self,
        database: &str,
        table: &str,
    ) -> Result<Vec<ForeignKeyInfo>> {
        let rs = self
            .rows(format!(
                "PRAGMA {}.foreign_key_list({})",
                D.quote_ident(database),
                D.quote_str(table)
            ))
            .await?;
        let mut out: Vec<(i64, ForeignKeyInfo)> = Vec::new();
        for r in &rs.rows {
            let id = int(&rs, r, "id");
            if out.last().is_none_or(|(last, _)| *last != id) {
                out.push((
                    id,
                    ForeignKeyInfo {
                        // SQLite foreign keys are usually unnamed.
                        name: format!("fk_{table}_{id}"),
                        ref_database: database.to_owned(),
                        ref_table: text(&rs, r, "table"),
                        on_update: text(&rs, r, "on_update"),
                        on_delete: text(&rs, r, "on_delete"),
                        ..ForeignKeyInfo::default()
                    },
                ));
            }
            let fk = &mut out.last_mut().expect("pushed above").1;
            fk.columns.push(text(&rs, r, "from"));
            fk.ref_columns.push(text(&rs, r, "to"));
        }
        Ok(out.into_iter().map(|(_, fk)| fk).collect())
    }

    async fn table_options(&mut self, _database: &str, _table: &str) -> Result<TableOptions> {
        Ok(TableOptions::default())
    }

    async fn list_checks(&mut self, _database: &str, _table: &str) -> Result<Vec<CheckInfo>> {
        // SQLite keeps checks only inside the table's CREATE statement.
        Ok(Vec::new())
    }

    async fn object_ddl(&mut self, database: &str, name: &str, kind: ObjectKind) -> Result<String> {
        let kind_name = match kind {
            ObjectKind::Table => "table",
            ObjectKind::View => "view",
            ObjectKind::Trigger => "trigger",
            _ => {
                return Err(Error::Unsupported(format!(
                    "SQLite has no {}",
                    kind.sql_keyword().to_lowercase()
                )));
            }
        };
        let rs = self
            .rows(format!(
                "SELECT sql FROM {}.sqlite_master WHERE type = '{kind_name}' AND name = {}",
                D.quote_ident(database),
                D.quote_str(name)
            ))
            .await?;
        rs.rows
            .first()
            .map(|r| text(&rs, r, "sql"))
            .ok_or_else(|| Error::Other(format!("{name} not found")))
    }

    async fn close(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_defaults() {
        assert_eq!(unquote_default("'it''s'"), "it's");
        assert_eq!(unquote_default("42"), "42");
        assert_eq!(unquote_default("CURRENT_TIMESTAMP"), "CURRENT_TIMESTAMP");
    }
}
