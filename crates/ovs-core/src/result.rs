use std::time::Duration;

use crate::value::Value;

/// Metadata for one result column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Column {
    /// Label shown in the grid header (the alias if one was used).
    pub name: String,
    /// Underlying column name, empty for expressions.
    pub origin_name: String,
    /// Source table, empty for expressions or multi-table derived columns.
    pub table: String,
    /// Source schema/database.
    pub schema: String,
    /// Engine type name, e.g. `VARCHAR`, `INT UNSIGNED`.
    pub type_name: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub unique_key: bool,
    pub binary: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// True when the driver stopped reading because the row limit was reached.
    pub truncated: bool,
}

/// What a single statement produced on the server.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryOutcome {
    Rows(ResultSet),
    Affected {
        rows: u64,
        last_insert_id: Option<u64>,
        warnings: u16,
        info: String,
    },
}

/// Everything the Messages tab needs to know about one executed statement.
#[derive(Debug, Clone)]
pub struct StatementReport {
    pub sql: String,
    pub outcomes: Vec<QueryOutcome>,
    pub elapsed: Duration,
    pub error: Option<String>,
}

impl StatementReport {
    pub fn result_sets(&self) -> impl Iterator<Item = &ResultSet> {
        self.outcomes.iter().filter_map(|o| match o {
            QueryOutcome::Rows(rs) => Some(rs),
            QueryOutcome::Affected { .. } => None,
        })
    }

    /// One-line summary in the style "3 row(s) returned" / "1 row(s) affected".
    pub fn summary(&self) -> String {
        if let Some(err) = &self.error {
            return err.clone();
        }
        let parts: Vec<String> = self
            .outcomes
            .iter()
            .map(|o| match o {
                QueryOutcome::Rows(rs) => {
                    let more = if rs.truncated { " (limited)" } else { "" };
                    format!("{} row(s) returned{more}", rs.rows.len())
                }
                QueryOutcome::Affected { rows, warnings, .. } => {
                    if *warnings > 0 {
                        format!("{rows} row(s) affected, {warnings} warning(s)")
                    } else {
                        format!("{rows} row(s) affected")
                    }
                }
            })
            .collect();
        if parts.is_empty() {
            "OK".to_owned()
        } else {
            parts.join("; ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(outcomes: Vec<QueryOutcome>) -> StatementReport {
        StatementReport {
            sql: String::new(),
            outcomes,
            elapsed: Duration::ZERO,
            error: None,
        }
    }

    #[test]
    fn summary_describes_each_outcome() {
        let rs = ResultSet {
            rows: vec![vec![Value::Int(1)], vec![Value::Int(2)]],
            ..ResultSet::default()
        };
        let r = report(vec![
            QueryOutcome::Rows(rs),
            QueryOutcome::Affected {
                rows: 4,
                last_insert_id: None,
                warnings: 1,
                info: String::new(),
            },
        ]);
        assert_eq!(
            r.summary(),
            "2 row(s) returned; 4 row(s) affected, 1 warning(s)"
        );
        assert_eq!(r.result_sets().count(), 1);
    }

    #[test]
    fn summary_prefers_error() {
        let mut r = report(vec![]);
        assert_eq!(r.summary(), "OK");
        r.error = Some("Error 1064: syntax".into());
        assert_eq!(r.summary(), "Error 1064: syntax");
    }
}
