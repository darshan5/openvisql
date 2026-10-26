//! SQL text utilities. Everything here works on plain strings so it is usable from the editor
//! (highlighting, statement-under-cursor) as well as from drivers.

mod analyze;
mod dialect;
mod edit;
pub mod keywords;
mod lexer;
mod splitter;

pub use analyze::{
    LimitClause, StatementKind, apply_row_limit, classify, count_query, limit_clause, missing_where,
};
pub use dialect::Dialect;
pub use edit::{CellEdit, RowEdit, RowKey, generate_edit};
pub use lexer::{Token, TokenKind, tokenize};
pub use splitter::{Statement, split_statements, statement_at};
