//! Engine-agnostic building blocks shared by every OpenViSQL driver and client.
//!
//! - [`connection`]: saved connection settings.
//! - [`driver`]: the async traits each database engine implements.
//! - [`result`] / [`value`]: query results as the UI consumes them.
//! - [`schema`]: objects shown in the object browser.
//! - [`dump`]: SQL dumps of a database and running scripts to restore them.
//! - [`editing`]: deciding how grid edits are written back, and pending changes.
//! - [`import`]: loading CSV files into tables.
//! - [`export`]: CSV, JSON, SQL, Markdown and HTML output of result sets.
//! - [`sql`]: lexing, statement splitting, quoting and grid-edit SQL generation.

pub mod connection;
pub mod driver;
pub mod dump;
pub mod editing;
pub mod error;
pub mod export;
pub mod import;
pub mod result;
pub mod schema;
pub mod sql;
pub mod value;

pub use connection::{ConnectionConfig, Engine, PasswordStore, SshAuthMethod, SshConfig, SslMode};
pub use driver::{CancelHandle, Driver, Session};
pub use error::{Error, Result};
pub use result::{Column, QueryOutcome, ResultSet, StatementReport};
pub use schema::{
    CheckInfo, ColumnInfo, ForeignKeyInfo, IndexInfo, ObjectKind, SchemaObject, TableOptions,
};
pub use value::Value;
