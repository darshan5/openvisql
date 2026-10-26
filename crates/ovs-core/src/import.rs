//! Importing CSV files into a table with batched `INSERT`s in one transaction.

use crate::driver::Session;
use crate::error::{Error, Result};
use crate::schema::ColumnInfo;
use crate::sql::Dialect;
use crate::value::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvOptions {
    pub delimiter: u8,
    /// The first line holds column names, matched to the table's columns.
    pub has_header: bool,
    /// A field equal to this is stored as NULL.
    pub null_text: String,
    /// Empty fields are stored as NULL rather than empty strings.
    pub empty_is_null: bool,
    pub rows_per_insert: usize,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            has_header: true,
            null_text: "NULL".to_owned(),
            empty_is_null: false,
            rows_per_insert: 500,
        }
    }
}

/// Parsed CSV contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvData {
    pub header: Option<Vec<String>>,
    pub rows: Vec<Vec<String>>,
}

/// Which CSV field goes into which table column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    /// (CSV field index, table column name).
    pub columns: Vec<(usize, String)>,
    /// CSV header names with no matching table column.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportStats {
    pub rows: u64,
}

pub fn parse_csv(text: &str, options: &CsvOptions) -> Result<CsvData> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(options.delimiter)
        .has_headers(options.has_header)
        .flexible(true)
        .from_reader(text.as_bytes());
    let header = if options.has_header {
        let names = reader
            .headers()
            .map_err(|e| Error::Other(format!("CSV header: {e}")))?;
        // A UTF-8 byte-order mark would otherwise end up in the first column's name.
        Some(
            names
                .iter()
                .map(|h| h.trim_start_matches('\u{feff}').trim().to_owned())
                .collect(),
        )
    } else {
        None
    };
    let mut rows = Vec::new();
    for (i, record) in reader.records().enumerate() {
        let record = record.map_err(|e| Error::Other(format!("CSV line {}: {e}", i + 2)))?;
        rows.push(record.iter().map(str::to_owned).collect());
    }
    Ok(CsvData { header, rows })
}

/// Matches CSV fields to table columns: by name (case-insensitive) with a header, otherwise by
/// position. Generated columns are never written.
pub fn plan(data: &CsvData, table_columns: &[ColumnInfo]) -> Result<ImportPlan> {
    let writable: Vec<&ColumnInfo> = table_columns
        .iter()
        .filter(|c| !c.extra.to_ascii_uppercase().contains("GENERATED"))
        .collect();
    let mut plan = ImportPlan {
        columns: Vec::new(),
        skipped: Vec::new(),
    };
    if let Some(header) = &data.header {
        for (i, name) in header.iter().enumerate() {
            match writable.iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
                Some(col) => plan.columns.push((i, col.name.clone())),
                None => plan.skipped.push(name.clone()),
            }
        }
    } else {
        let width = data.rows.iter().map(Vec::len).max().unwrap_or(0);
        for (i, col) in writable.iter().enumerate().take(width) {
            plan.columns.push((i, col.name.clone()));
        }
    }
    if plan.columns.is_empty() {
        return Err(Error::Other(
            "no CSV column matches a column of the table".into(),
        ));
    }
    Ok(plan)
}

fn field_value(field: Option<&String>, options: &CsvOptions) -> Value {
    match field {
        None => Value::Null,
        Some(f) if *f == options.null_text => Value::Null,
        Some(f) if f.is_empty() && options.empty_is_null => Value::Null,
        Some(f) => Value::Text(f.clone()),
    }
}

/// The batched `INSERT` statements for the import.
pub fn insert_statements(
    dialect: Dialect,
    db: &str,
    table: &str,
    plan: &ImportPlan,
    rows: &[Vec<String>],
    options: &CsvOptions,
) -> Vec<String> {
    let cols = plan
        .columns
        .iter()
        .map(|(_, name)| dialect.quote_ident(name))
        .collect::<Vec<_>>()
        .join(", ");
    let target = dialect.qualified(db, table);
    rows.chunks(options.rows_per_insert.max(1))
        .map(|chunk| {
            let values = chunk
                .iter()
                .map(|row| {
                    let vals: Vec<String> = plan
                        .columns
                        .iter()
                        .map(|(i, _)| dialect.literal(&field_value(row.get(*i), options)))
                        .collect();
                    format!("({})", vals.join(", "))
                })
                .collect::<Vec<_>>()
                .join(",\n");
            format!("INSERT INTO {target} ({cols}) VALUES\n{values}")
        })
        .collect()
}

/// Imports CSV `text` into `db.table` in one transaction; any failure rolls everything back.
pub async fn import_csv(
    session: &mut dyn Session,
    db: &str,
    table: &str,
    text: &str,
    options: &CsvOptions,
) -> Result<(ImportStats, ImportPlan)> {
    let data = parse_csv(text, options)?;
    let columns = session.list_columns(db, table).await?;
    let plan = plan(&data, &columns)?;
    let statements = insert_statements(session.dialect(), db, table, &plan, &data.rows, options);
    session.execute("START TRANSACTION", None).await?;
    for (i, sql) in statements.iter().enumerate() {
        if let Err(err) = session.execute(sql, None).await {
            let _ = session.execute("ROLLBACK", None).await;
            let first = i * options.rows_per_insert.max(1) + 1;
            return Err(Error::Other(format!(
                "rows {first}–{} could not be inserted, nothing was imported: {err}",
                (first + options.rows_per_insert.max(1) - 1).min(data.rows.len())
            )));
        }
    }
    session.execute("COMMIT", None).await?;
    Ok((
        ImportStats {
            rows: data.rows.len() as u64,
        },
        plan,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            ..ColumnInfo::default()
        }
    }

    #[test]
    fn matches_header_names_and_skips_unknown() {
        let text = "\u{feff}Name,unknown,ID\nAda,x,1\n\"Lovelace, A.\",y,2\n";
        let data = parse_csv(text, &CsvOptions::default()).unwrap();
        assert_eq!(data.rows.len(), 2);
        assert_eq!(data.rows[1][0], "Lovelace, A.");
        let plan = plan(&data, &[col("id"), col("name")]).unwrap();
        assert_eq!(plan.columns, [(0, "name".to_owned()), (2, "id".to_owned())]);
        assert_eq!(plan.skipped, ["unknown"]);
    }

    #[test]
    fn positional_without_header_and_nulls() {
        let options = CsvOptions {
            has_header: false,
            delimiter: b';',
            empty_is_null: true,
            rows_per_insert: 2,
            ..CsvOptions::default()
        };
        let data = parse_csv("1;NULL\n2;\n3;it's\n", &options).unwrap();
        let plan = plan(&data, &[col("a"), col("b"), col("c")]).unwrap();
        assert_eq!(plan.columns.len(), 2);
        let sql = insert_statements(Dialect::MySql, "d", "t", &plan, &data.rows, &options);
        assert_eq!(
            sql,
            [
                "INSERT INTO `d`.`t` (`a`, `b`) VALUES\n('1', NULL),\n('2', NULL)",
                "INSERT INTO `d`.`t` (`a`, `b`) VALUES\n('3', 'it\\'s')",
            ]
        );
    }

    #[test]
    fn generated_columns_and_no_match_are_rejected() {
        let generated = ColumnInfo {
            extra: "VIRTUAL GENERATED".into(),
            ..col("half")
        };
        let data = parse_csv("half\n1\n", &CsvOptions::default()).unwrap();
        assert!(plan(&data, &[col("id"), generated]).is_err());
    }
}
