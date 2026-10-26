//! SQL text utilities. Everything here works on plain strings so it is usable from the editor
//! (highlighting, statement-under-cursor) as well as from drivers.

mod analyze;
pub mod complete;
pub mod design;
mod dialect;
mod edit;
pub mod keywords;
mod lexer;
pub mod paging;
mod splitter;
pub mod templates;
pub mod text;
pub mod users;

pub use analyze::{
    LimitClause, StatementKind, apply_row_limit, classify, count_query, limit_clause,
    missing_where, strip_limit,
};
pub use dialect::Dialect;
pub use edit::{CellEdit, RowEdit, RowKey, generate_edit};
pub use lexer::{Token, TokenKind, tokenize, tokenize_for};
pub use splitter::{
    Statement, split_statements, split_statements_for, statement_at, statement_at_for,
};
