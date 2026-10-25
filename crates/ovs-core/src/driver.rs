use std::sync::Arc;

use async_trait::async_trait;

use crate::connection::{ConnectionConfig, Engine};
use crate::error::Result;
use crate::result::QueryOutcome;
use crate::schema::{ColumnInfo, IndexInfo, ObjectKind, SchemaObject};
use crate::sql::Dialect;

/// Entry point for one database engine.
#[async_trait]
pub trait Driver: Send + Sync {
    fn engine(&self) -> Engine;

    async fn connect(
        &self,
        config: &ConnectionConfig,
        password: Option<&str>,
    ) -> Result<Box<dyn Session>>;
}

/// An open connection. Sessions are used by one task at a time; run concurrent work (e.g. the
/// object browser next to a long query) on separate sessions.
#[async_trait]
pub trait Session: Send {
    fn dialect(&self) -> Dialect;

    /// Version string reported by the server, e.g. `11.4.2-MariaDB`.
    fn server_version(&self) -> &str;

    /// A handle that can abort the statement currently running on this session from another task.
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;

    /// Runs one statement (or a server-side multi-statement batch) and collects every result.
    /// When `row_limit` is set, reading stops after that many rows per result set and the set is
    /// marked truncated.
    async fn execute(&mut self, sql: &str, row_limit: Option<usize>) -> Result<Vec<QueryOutcome>>;

    async fn ping(&mut self) -> Result<()>;

    async fn current_database(&mut self) -> Result<Option<String>>;
    async fn use_database(&mut self, database: &str) -> Result<()>;

    async fn list_databases(&mut self) -> Result<Vec<String>>;
    async fn list_objects(&mut self, database: &str, kind: ObjectKind)
    -> Result<Vec<SchemaObject>>;
    async fn list_columns(&mut self, database: &str, table: &str) -> Result<Vec<ColumnInfo>>;
    async fn list_indexes(&mut self, database: &str, table: &str) -> Result<Vec<IndexInfo>>;

    /// The `CREATE ...` statement for an object.
    async fn object_ddl(&mut self, database: &str, name: &str, kind: ObjectKind) -> Result<String>;

    async fn close(self: Box<Self>) -> Result<()>;
}

#[async_trait]
pub trait CancelHandle: Send + Sync {
    async fn cancel(&self) -> Result<()>;
}
