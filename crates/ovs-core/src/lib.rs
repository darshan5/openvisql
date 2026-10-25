//! Engine-agnostic building blocks shared by every OpenViSQL driver and client.
//!
//! - [`connection`]: saved connection settings.
//! - [`driver`]: the async traits each database engine implements.
//! - [`result`] / [`value`]: query results as the UI consumes them.
//! - [`schema`]: objects shown in the object browser.
//! - [`editing`]: deciding how grid edits are written back, and pending changes.
//! - [`sql`]: lexing, statement splitting, quoting and grid-edit SQL generation.

pub mod connection;
pub mod driver;
pub mod editing;
pub mod error;
pub mod result;
pub mod schema;
pub mod sql;
pub mod value;

pub use connection::{ConnectionConfig, Engine, SslMode};
pub use driver::{CancelHandle, Driver, Session};
pub use error::{Error, Result};
pub use result::{Column, QueryOutcome, ResultSet, StatementReport};
pub use schema::{ColumnInfo, IndexInfo, ObjectKind, SchemaObject};
pub use value::Value;
