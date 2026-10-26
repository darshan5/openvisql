//! MySQL and MariaDB support, built on `mysql_async`.

mod convert;
mod introspect;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};
use ovs_core::sql::Dialect;
use ovs_core::{
    CancelHandle, ColumnInfo, ConnectionConfig, Driver, Engine, Error, ForeignKeyInfo, IndexInfo,
    ObjectKind, QueryOutcome, Result, ResultSet, SchemaObject, Session, SslMode, TableOptions,
};

/// Server error raised in the session whose statement was killed by `KILL QUERY`.
const ER_QUERY_INTERRUPTED: u16 = 1317;

#[derive(Debug, Default, Clone, Copy)]
pub struct MySqlDriver;

#[async_trait]
impl Driver for MySqlDriver {
    fn engine(&self) -> Engine {
        Engine::MySql
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        password: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        let opts = build_opts(config, password);
        let timeout = Duration::from_secs(config.connect_timeout_secs.max(1));
        let mut conn = tokio_timeout(timeout, Conn::new(opts.clone()))
            .await?
            .map_err(map_err)?;
        let version: Option<String> = conn
            .query_first("SELECT VERSION()")
            .await
            .map_err(map_err)?;
        Ok(Box::new(MySqlSession {
            cancel: Arc::new(MySqlCancel {
                opts,
                connection_id: conn.id(),
            }),
            conn,
            version: version.unwrap_or_default(),
        }))
    }
}

async fn tokio_timeout<F: std::future::Future>(dur: Duration, fut: F) -> Result<F::Output> {
    tokio::time::timeout(dur, fut)
        .await
        .map_err(|_| Error::Connection(format!("timed out after {}s", dur.as_secs())))
}

fn build_opts(config: &ConnectionConfig, password: Option<&str>) -> Opts {
    let init: Vec<String> = config
        .init_commands
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let ssl = match config.ssl {
        SslMode::Disabled => None,
        SslMode::Preferred => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        SslMode::Required => {
            let mut opts = SslOpts::default();
            if let Some(ca) = config.ssl_ca.as_deref().filter(|s| !s.is_empty()) {
                opts = opts.with_root_certs(vec![std::path::PathBuf::from(ca).into()]);
            }
            Some(opts)
        }
    };

    let builder = OptsBuilder::default()
        .ip_or_hostname(config.host.clone())
        .tcp_port(config.port)
        .user(Some(config.user.clone()))
        .pass(password)
        .init(init)
        .ssl_opts(ssl)
        .prefer_socket(false)
        // Text protocol only; cached prepared statements would leak server memory per tab.
        .stmt_cache_size(0)
        .compression(config.compress.then(mysql_async::Compression::default));
    builder.into()
}

pub(crate) fn map_err(err: mysql_async::Error) -> Error {
    match err {
        mysql_async::Error::Server(e) if e.code == ER_QUERY_INTERRUPTED => Error::Cancelled,
        mysql_async::Error::Server(e) => Error::server(u32::from(e.code), e.message),
        mysql_async::Error::Io(e) => Error::Connection(e.to_string()),
        mysql_async::Error::Driver(e) => Error::Connection(e.to_string()),
        other => Error::Other(other.to_string()),
    }
}

pub struct MySqlSession {
    conn: Conn,
    version: String,
    cancel: Arc<MySqlCancel>,
}

impl MySqlSession {
    /// Runs an internal query and returns its first result set.
    pub(crate) async fn query_set(&mut self, sql: &str) -> Result<ResultSet> {
        let outcomes = self.execute(sql, None).await?;
        outcomes
            .into_iter()
            .find_map(|o| match o {
                QueryOutcome::Rows(rs) => Some(rs),
                QueryOutcome::Affected { .. } => None,
            })
            .ok_or_else(|| Error::Other(format!("expected a result set from: {sql}")))
    }

    pub fn is_mariadb(&self) -> bool {
        self.version.to_ascii_lowercase().contains("mariadb")
    }
}

#[async_trait]
impl Session for MySqlSession {
    fn dialect(&self) -> Dialect {
        Dialect::MySql
    }

    fn server_version(&self) -> &str {
        &self.version
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    async fn execute(&mut self, sql: &str, row_limit: Option<usize>) -> Result<Vec<QueryOutcome>> {
        let mut result = self.conn.query_iter(sql).await.map_err(map_err)?;
        let mut outcomes = Vec::new();
        loop {
            let columns = result.columns().unwrap_or_else(|| Arc::from([]));
            if columns.is_empty() {
                outcomes.push(QueryOutcome::Affected {
                    rows: result.affected_rows(),
                    last_insert_id: result.last_insert_id(),
                    warnings: result.warnings(),
                    info: result.info().into_owned(),
                });
                // Consumes the OK packet and moves to the next result, if any.
                result.next().await.map_err(map_err)?;
            } else {
                let mut set = ResultSet {
                    columns: columns.iter().map(convert::column).collect(),
                    ..ResultSet::default()
                };
                while let Some(row) = result.next().await.map_err(map_err)? {
                    if row_limit.is_some_and(|limit| set.rows.len() >= limit) {
                        // Keep draining so the connection stays usable, but drop the data.
                        set.truncated = true;
                        continue;
                    }
                    let values = row.unwrap_raw();
                    set.rows.push(
                        values
                            .into_iter()
                            .zip(columns.iter())
                            .map(|(v, col)| convert::value(v, col))
                            .collect(),
                    );
                }
                outcomes.push(QueryOutcome::Rows(set));
            }
            if result.is_empty() {
                break;
            }
        }
        Ok(outcomes)
    }

    async fn ping(&mut self) -> Result<()> {
        self.conn.ping().await.map_err(map_err)
    }

    async fn current_database(&mut self) -> Result<Option<String>> {
        let db: Option<Option<String>> = self
            .conn
            .query_first("SELECT DATABASE()")
            .await
            .map_err(map_err)?;
        Ok(db.flatten())
    }

    async fn use_database(&mut self, database: &str) -> Result<()> {
        let sql = format!("USE {}", Dialect::MySql.quote_ident(database));
        self.conn.query_drop(sql).await.map_err(map_err)
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        introspect::databases(self).await
    }

    async fn list_objects(
        &mut self,
        database: &str,
        kind: ObjectKind,
    ) -> Result<Vec<SchemaObject>> {
        introspect::objects(self, database, kind).await
    }

    async fn list_columns(&mut self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        introspect::columns(self, database, table).await
    }

    async fn list_indexes(&mut self, database: &str, table: &str) -> Result<Vec<IndexInfo>> {
        introspect::indexes(self, database, table).await
    }

    async fn list_foreign_keys(
        &mut self,
        database: &str,
        table: &str,
    ) -> Result<Vec<ForeignKeyInfo>> {
        introspect::foreign_keys(self, database, table).await
    }

    async fn table_options(&mut self, database: &str, table: &str) -> Result<TableOptions> {
        introspect::table_options(self, database, table).await
    }

    async fn object_ddl(&mut self, database: &str, name: &str, kind: ObjectKind) -> Result<String> {
        introspect::ddl(self, database, name, kind).await
    }

    async fn close(self: Box<Self>) -> Result<()> {
        self.conn.disconnect().await.map_err(map_err)
    }
}

/// Cancels by opening a short-lived second connection and issuing `KILL QUERY`, which is the
/// only way the MySQL protocol offers to interrupt a running statement.
struct MySqlCancel {
    opts: Opts,
    connection_id: u32,
}

#[async_trait]
impl CancelHandle for MySqlCancel {
    async fn cancel(&self) -> Result<()> {
        let mut conn = Conn::new(self.opts.clone()).await.map_err(map_err)?;
        let killed = conn
            .query_drop(format!("KILL QUERY {}", self.connection_id))
            .await
            .map_err(map_err);
        conn.disconnect().await.map_err(map_err)?;
        killed
    }
}
