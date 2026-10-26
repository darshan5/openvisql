//! PostgreSQL support, built on `tokio-postgres`.
//!
//! A connection is to one database; its schemas play the part of MySQL's databases in the
//! object browser, and "using" one sets the `search_path`.
//!
//! Statements are prepared first (without running) to learn column types and source tables,
//! then run with the simple query protocol, which returns every value as text in PostgreSQL's
//! own formatting, so any type displays and round-trips.

mod introspect;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use ovs_core::sql::Dialect;
use ovs_core::{
    CancelHandle, CheckInfo, Column, ColumnInfo, ConnectionConfig, Driver, Engine, Error,
    ForeignKeyInfo, IndexInfo, ObjectKind, QueryOutcome, Result, ResultSet, SchemaObject, Session,
    SslMode, TableOptions, Value,
};
use tokio_postgres::types::Type;
use tokio_postgres::{CancelToken, Client, NoTls, SimpleQueryMessage};

const D: Dialect = Dialect::Postgres;

#[derive(Debug, Default, Clone, Copy)]
pub struct PostgresDriver;

#[async_trait]
impl Driver for PostgresDriver {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        password: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        if config.ssl != SslMode::Disabled {
            return Err(Error::Unsupported(
                "SSL for PostgreSQL connections isn't available yet; turn SSL off or use an \
                 SSH tunnel"
                    .into(),
            ));
        }
        let mut pg = tokio_postgres::Config::new();
        pg.host(&config.host)
            .port(config.port)
            .user(&config.user)
            .dbname(if config.database.trim().is_empty() {
                "postgres"
            } else {
                config.database.trim()
            })
            .application_name("OpenViSQL")
            .connect_timeout(Duration::from_secs(config.connect_timeout_secs.max(1)));
        if let Some(password) = password {
            pg.password(password);
        }
        let (client, connection) = pg.connect(NoTls).await.map_err(map_err)?;
        // The connection object drives the socket until the client is dropped.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        if !config.init_commands.trim().is_empty() {
            client
                .batch_execute(&config.init_commands)
                .await
                .map_err(map_err)?;
        }
        let version = client
            .simple_query("SHOW server_version")
            .await
            .map_err(map_err)?
            .into_iter()
            .find_map(|m| match m {
                SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
                _ => None,
            })
            .unwrap_or_default();
        let current = first_text(&client, "SELECT current_schema()")
            .await?
            .unwrap_or_else(|| "public".to_owned());
        Ok(Box::new(PostgresSession {
            cancel: Arc::new(PostgresCancel(client.cancel_token())),
            client,
            version: format!("PostgreSQL {version}"),
            current,
            origins: HashMap::new(),
        }))
    }
}

async fn first_text(client: &Client, sql: &str) -> Result<Option<String>> {
    Ok(client
        .simple_query(sql)
        .await
        .map_err(map_err)?
        .into_iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row.get(0).map(str::to_owned)),
            _ => None,
        })
        .flatten())
}

// Takes the error by value so it can be passed straight to `map_err`.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn map_err(err: tokio_postgres::Error) -> Error {
    if let Some(db) = err.as_db_error() {
        if *db.code() == tokio_postgres::error::SqlState::QUERY_CANCELED {
            return Error::Cancelled;
        }
        let mut message = format!("{} ({})", db.message(), db.code().code());
        if let Some(detail) = db.detail() {
            message.push('\n');
            message.push_str(detail);
        }
        if let Some(hint) = db.hint() {
            message.push_str("\nHint: ");
            message.push_str(hint);
        }
        return Error::server(None, message);
    }
    if err.is_closed() {
        return Error::Connection("the connection was closed".into());
    }
    Error::Connection(err.to_string())
}

struct PostgresCancel(CancelToken);

#[async_trait]
impl CancelHandle for PostgresCancel {
    async fn cancel(&self) -> Result<()> {
        self.0.cancel_query(NoTls).await.map_err(map_err)
    }
}

/// Where a result column comes from: (schema, table, column, NOT NULL).
type Origin = (String, String, String, bool);

pub struct PostgresSession {
    client: Client,
    version: String,
    cancel: Arc<PostgresCancel>,
    /// The schema `USE` picked, first on the `search_path`.
    current: String,
    /// Source columns by (table oid, column number), looked up once per table.
    origins: HashMap<(u32, i16), Origin>,
}

/// A value as the simple query protocol sends it (text), typed by the column's type.
fn convert(text: Option<&str>, ty: Option<&Type>) -> Value {
    let Some(text) = text else {
        return Value::Null;
    };
    match ty {
        Some(&Type::INT2 | &Type::INT4 | &Type::INT8 | &Type::OID) => text
            .parse()
            .map_or_else(|_| Value::Text(text.to_owned()), Value::Int),
        Some(&Type::FLOAT4 | &Type::FLOAT8) => text
            .parse()
            .map_or_else(|_| Value::Text(text.to_owned()), Value::Float),
        Some(&Type::BOOL) => Value::Text(if text == "t" { "true" } else { "false" }.to_owned()),
        Some(&Type::BYTEA) => text
            .strip_prefix("\\x")
            .and_then(decode_hex)
            .map_or_else(|| Value::Text(text.to_owned()), Value::Bytes),
        _ => Value::Text(text.to_owned()),
    }
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

impl PostgresSession {
    /// Runs an internal query and returns its first result set.
    pub(crate) async fn rows(&mut self, sql: &str) -> Result<ResultSet> {
        Ok(self
            .execute(sql, None)
            .await?
            .into_iter()
            .find_map(|o| match o {
                QueryOutcome::Rows(rs) => Some(rs),
                QueryOutcome::Affected { .. } => None,
            })
            .unwrap_or_default())
    }

    /// Fills in which table column each result column comes from.
    async fn resolve_origins(&mut self, keys: &[(u32, i16)]) -> Result<()> {
        let missing: Vec<u32> = keys
            .iter()
            .filter(|k| !self.origins.contains_key(k))
            .map(|(oid, _)| *oid)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let list = missing
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT c.oid, n.nspname, c.relname, a.attnum, a.attname, a.attnotnull \
             FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE a.attrelid IN ({list}) AND a.attnum > 0"
        );
        for message in self.client.simple_query(&sql).await.map_err(map_err)? {
            if let SimpleQueryMessage::Row(row) = message {
                let text = |i: usize| row.get(i).unwrap_or_default().to_owned();
                let (Ok(oid), Ok(attnum)) = (text(0).parse(), text(3).parse()) else {
                    continue;
                };
                self.origins
                    .insert((oid, attnum), (text(1), text(2), text(4), text(5) == "t"));
            }
        }
        Ok(())
    }

    async fn run(&mut self, sql: &str, row_limit: Option<usize>) -> Result<Vec<QueryOutcome>> {
        // Preparing describes the result without running anything. A script of several
        // statements can't be prepared; it runs untyped.
        let prepared = match self.client.prepare(sql).await {
            Ok(statement) => Some(statement),
            Err(err)
                if err
                    .as_db_error()
                    .is_some_and(|db| db.message().contains("multiple commands")) =>
            {
                None
            }
            Err(err) => return Err(map_err(err)),
        };
        let typed: Vec<(Type, Option<(u32, i16)>)> = prepared
            .as_ref()
            .map(|s| {
                s.columns()
                    .iter()
                    .map(|c| {
                        let origin = c.table_oid().zip(c.column_id()).filter(|(t, _)| *t != 0);
                        (c.type_().clone(), origin)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let keys: Vec<(u32, i16)> = typed.iter().filter_map(|(_, o)| *o).collect();
        if !keys.is_empty() {
            self.resolve_origins(&keys).await?;
        }
        let described = prepared.is_some().then_some(typed);

        let mut stream = Box::pin(self.client.simple_query_raw(sql).await.map_err(map_err)?);
        let mut out = Vec::new();
        let mut current: Option<ResultSet> = None;
        let mut statement = 0;
        while let Some(message) = stream.next().await {
            match message.map_err(map_err)? {
                SimpleQueryMessage::RowDescription(columns) => {
                    let column_types = described.as_ref().filter(|_| statement == 0);
                    let columns = columns
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            let (ty, origin) = column_types
                                .and_then(|t| t.get(i))
                                .map_or((None, None), |(ty, o)| (Some(ty), *o));
                            let mut column = Column {
                                name: c.name().to_owned(),
                                type_name: ty
                                    .map(|t| t.name().to_ascii_uppercase())
                                    .unwrap_or_default(),
                                binary: ty == Some(&Type::BYTEA),
                                nullable: true,
                                ..Column::default()
                            };
                            if let Some((schema, table, name, not_null)) =
                                origin.and_then(|k| self.origins.get(&k))
                            {
                                column.schema.clone_from(schema);
                                column.table.clone_from(table);
                                column.origin_name.clone_from(name);
                                column.nullable = !not_null;
                            }
                            column
                        })
                        .collect();
                    current = Some(ResultSet {
                        columns,
                        rows: Vec::new(),
                        truncated: false,
                    });
                }
                SimpleQueryMessage::Row(row) => {
                    let Some(set) = current.as_mut() else {
                        continue;
                    };
                    if row_limit.is_some_and(|limit| set.rows.len() >= limit) {
                        set.truncated = true;
                        continue;
                    }
                    let column_types = described.as_ref().filter(|_| statement == 0);
                    let values = (0..row.len())
                        .map(|i| {
                            convert(
                                row.get(i),
                                column_types.and_then(|t| t.get(i)).map(|(t, _)| t),
                            )
                        })
                        .collect();
                    set.rows.push(values);
                }
                SimpleQueryMessage::CommandComplete(rows) => {
                    statement += 1;
                    match current.take() {
                        Some(set) => out.push(QueryOutcome::Rows(set)),
                        None => out.push(QueryOutcome::Affected {
                            rows,
                            last_insert_id: None,
                            warnings: 0,
                            info: String::new(),
                        }),
                    }
                }
                _ => {}
            }
        }
        // Schema changes can move columns around, so forget what was looked up.
        if out
            .iter()
            .any(|o| matches!(o, QueryOutcome::Affected { .. }))
        {
            self.origins.clear();
        }
        Ok(out)
    }
}

#[async_trait]
impl Session for PostgresSession {
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
        self.run(sql, row_limit).await
    }

    async fn ping(&mut self) -> Result<()> {
        self.client
            .simple_query("SELECT 1")
            .await
            .map(|_| ())
            .map_err(map_err)
    }

    async fn current_database(&mut self) -> Result<Option<String>> {
        Ok(Some(self.current.clone()))
    }

    async fn use_database(&mut self, schema: &str) -> Result<()> {
        // Keep public reachable for its functions and extensions.
        let path = if schema == "public" {
            D.quote_ident(schema)
        } else {
            format!("{}, public", D.quote_ident(schema))
        };
        self.client
            .batch_execute(&format!("SET search_path TO {path}"))
            .await
            .map_err(map_err)?;
        schema.clone_into(&mut self.current);
        Ok(())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        introspect::schemas(self).await
    }

    async fn list_objects(&mut self, schema: &str, kind: ObjectKind) -> Result<Vec<SchemaObject>> {
        introspect::objects(self, schema, kind).await
    }

    async fn list_columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        introspect::columns(self, schema, table).await
    }

    async fn list_indexes(&mut self, schema: &str, table: &str) -> Result<Vec<IndexInfo>> {
        introspect::indexes(self, schema, table).await
    }

    async fn list_foreign_keys(
        &mut self,
        schema: &str,
        table: &str,
    ) -> Result<Vec<ForeignKeyInfo>> {
        introspect::foreign_keys(self, schema, table).await
    }

    async fn table_options(&mut self, schema: &str, table: &str) -> Result<TableOptions> {
        introspect::table_options(self, schema, table).await
    }

    async fn list_checks(&mut self, schema: &str, table: &str) -> Result<Vec<CheckInfo>> {
        introspect::checks(self, schema, table).await
    }

    async fn object_ddl(&mut self, schema: &str, name: &str, kind: ObjectKind) -> Result<String> {
        introspect::ddl(self, schema, name, kind).await
    }

    async fn close(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_text_values_by_type() {
        assert_eq!(convert(Some("42"), Some(&Type::INT8)), Value::Int(42));
        assert_eq!(
            convert(Some("t"), Some(&Type::BOOL)),
            Value::Text("true".into())
        );
        assert_eq!(
            convert(Some("\\x00ff"), Some(&Type::BYTEA)),
            Value::Bytes(vec![0, 0xff])
        );
        assert_eq!(
            convert(Some("1.50"), Some(&Type::NUMERIC)),
            Value::Text("1.50".into())
        );
        assert_eq!(convert(None, Some(&Type::TEXT)), Value::Null);
    }
}
