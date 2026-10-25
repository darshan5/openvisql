use super::dialect::Dialect;
use crate::value::Value;

/// How an edited row is located on the server.
#[derive(Debug, Clone, PartialEq)]
pub enum RowKey {
    /// Primary or unique key columns with their original values.
    Unique(Vec<(String, Value)>),
    /// No usable key: match every original column value and touch at most one row. Callers
    /// should leave out columns whose values don't compare reliably (floats, large BLOBs).
    AllColumns(Vec<(String, Value)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CellEdit {
    pub column: String,
    pub value: Value,
}

/// A pending change made in the result or table-data grid.
#[derive(Debug, Clone, PartialEq)]
pub enum RowEdit {
    Update { key: RowKey, changes: Vec<CellEdit> },
    Insert { values: Vec<CellEdit> },
    Delete { key: RowKey },
}

/// Builds the DML statement that applies `edit` to `schema.table`.
pub fn generate_edit(dialect: Dialect, schema: &str, table: &str, edit: &RowEdit) -> String {
    let target = dialect.qualified(schema, table);
    match edit {
        RowEdit::Update { key, changes } => {
            let set = changes
                .iter()
                .map(|c| {
                    format!(
                        "{} = {}",
                        dialect.quote_ident(&c.column),
                        dialect.literal(&c.value)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("UPDATE {target} SET {set}{}", where_clause(dialect, key))
        }
        RowEdit::Insert { values } => {
            let cols = values
                .iter()
                .map(|c| dialect.quote_ident(&c.column))
                .collect::<Vec<_>>()
                .join(", ");
            let vals = values
                .iter()
                .map(|c| dialect.literal(&c.value))
                .collect::<Vec<_>>()
                .join(", ");
            format!("INSERT INTO {target} ({cols}) VALUES ({vals})")
        }
        RowEdit::Delete { key } => {
            format!("DELETE FROM {target}{}", where_clause(dialect, key))
        }
    }
}

fn where_clause(dialect: Dialect, key: &RowKey) -> String {
    let (cols, limit) = match key {
        RowKey::Unique(cols) => (cols, ""),
        RowKey::AllColumns(cols) => (cols, " LIMIT 1"),
    };
    let preds = cols
        .iter()
        .map(|(name, value)| {
            let col = dialect.quote_ident(name);
            if value.is_null() {
                format!("{col} IS NULL")
            } else {
                format!("{col} = {}", dialect.literal(value))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    format!(" WHERE {preds}{limit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(column: &str, value: Value) -> CellEdit {
        CellEdit {
            column: column.into(),
            value,
        }
    }

    #[test]
    fn update_by_primary_key() {
        let edit = RowEdit::Update {
            key: RowKey::Unique(vec![("id".into(), Value::Int(4))]),
            changes: vec![
                cell("name", Value::Text("O'Hara".into())),
                cell("x", Value::Null),
            ],
        };
        assert_eq!(
            generate_edit(Dialect::MySql, "app", "users", &edit),
            r"UPDATE `app`.`users` SET `name` = 'O\'Hara', `x` = NULL WHERE `id` = 4"
        );
    }

    #[test]
    fn keyless_rows_match_all_columns_and_limit_one() {
        let edit = RowEdit::Delete {
            key: RowKey::AllColumns(vec![("a".into(), Value::Int(1)), ("b".into(), Value::Null)]),
        };
        assert_eq!(
            generate_edit(Dialect::MySql, "", "t", &edit),
            "DELETE FROM `t` WHERE `a` = 1 AND `b` IS NULL LIMIT 1"
        );
    }

    #[test]
    fn insert_row() {
        let edit = RowEdit::Insert {
            values: vec![
                cell("a", Value::UInt(1)),
                cell("b", Value::Text("x".into())),
            ],
        };
        assert_eq!(
            generate_edit(Dialect::MySql, "d", "t", &edit),
            "INSERT INTO `d`.`t` (`a`, `b`) VALUES (1, 'x')"
        );
    }
}
