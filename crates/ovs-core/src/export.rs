//! Writing result sets to files and the clipboard: CSV, JSON, SQL `INSERT`s, Markdown and
//! HTML.

use std::io::{self, Write};

use crate::result::ResultSet;
use crate::sql::Dialect;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    SqlInsert,
    Markdown,
    Html,
}

impl ExportFormat {
    pub const ALL: [Self; 5] = [
        Self::Csv,
        Self::Json,
        Self::SqlInsert,
        Self::Markdown,
        Self::Html,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Json => "JSON",
            Self::SqlInsert => "SQL INSERT",
            Self::Markdown => "Markdown",
            Self::Html => "HTML",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::SqlInsert => "sql",
            Self::Markdown => "md",
            Self::Html => "html",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// CSV field separator.
    pub delimiter: char,
    /// Write column names as the first CSV line.
    pub header: bool,
    /// How NULL is written in CSV and Markdown.
    pub null_text: String,
    /// Prefix CSV cells that start with `=`, `+`, `-` or `@` with `'`, so spreadsheets don't
    /// evaluate them as formulas.
    pub escape_formulas: bool,
    /// Table named in SQL `INSERT`s.
    pub table: String,
    /// Rows per `INSERT` statement.
    pub rows_per_insert: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Csv,
            delimiter: ',',
            header: true,
            null_text: String::new(),
            escape_formulas: false,
            table: "result".to_owned(),
            rows_per_insert: 100,
        }
    }
}

/// Full text of a value for export (the grid's `display()` truncates).
fn full_text(v: &Value) -> String {
    match v {
        Value::Bytes(b) => format!("0x{}", crate::value::hex(b)),
        v => v.to_string(),
    }
}

/// Writes `rs` in the chosen format.
pub fn export(rs: &ResultSet, options: &ExportOptions, out: &mut impl Write) -> io::Result<()> {
    match options.format {
        ExportFormat::Csv => csv(rs, options, out),
        ExportFormat::Json => json(rs, out),
        ExportFormat::SqlInsert => sql_insert(rs, options, out),
        ExportFormat::Markdown => markdown(rs, options, out),
        ExportFormat::Html => html(rs, out),
    }
}

/// Convenience wrapper returning the export as a string, for the clipboard.
pub fn export_to_string(rs: &ResultSet, options: &ExportOptions) -> String {
    let mut buf = Vec::new();
    export(rs, options, &mut buf).expect("writing to a Vec cannot fail");
    String::from_utf8_lossy(&buf).into_owned()
}

fn csv_field(text: &str, options: &ExportOptions) -> String {
    let guarded = if options.escape_formulas && text.starts_with(['=', '+', '-', '@']) {
        format!("'{text}")
    } else {
        text.to_owned()
    };
    let needs_quotes = guarded.contains(options.delimiter)
        || guarded.contains(['"', '\n', '\r'])
        || guarded.starts_with(' ')
        || guarded.ends_with(' ');
    if needs_quotes {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

fn csv(rs: &ResultSet, options: &ExportOptions, out: &mut impl Write) -> io::Result<()> {
    let sep = options.delimiter.to_string();
    if options.header {
        let names: Vec<String> = rs
            .columns
            .iter()
            .map(|c| csv_field(&c.name, options))
            .collect();
        writeln!(out, "{}", names.join(&sep))?;
    }
    for row in &rs.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| match v {
                Value::Null => options.null_text.clone(),
                v => csv_field(&full_text(v), options),
            })
            .collect();
        writeln!(out, "{}", cells.join(&sep))?;
    }
    Ok(())
}

fn json_string(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_value(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        // JSON has no NaN or infinity.
        Value::Null | Value::Float(_) => "null".to_owned(),
        Value::Text(s) => json_string(s),
        Value::Bytes(b) => json_string(&format!("0x{}", crate::value::hex(b))),
    }
}

/// An array of objects keyed by column name. DECIMAL and temporal values stay strings so no
/// precision is lost.
fn json(rs: &ResultSet, out: &mut impl Write) -> io::Result<()> {
    let names: Vec<String> = rs.columns.iter().map(|c| json_string(&c.name)).collect();
    writeln!(out, "[")?;
    for (i, row) in rs.rows.iter().enumerate() {
        let fields: Vec<String> = names
            .iter()
            .zip(row)
            .map(|(name, v)| format!("{name}: {}", json_value(v)))
            .collect();
        let comma = if i + 1 < rs.rows.len() { "," } else { "" };
        writeln!(out, "  {{{}}}{comma}", fields.join(", "))?;
    }
    writeln!(out, "]")
}

fn sql_insert(rs: &ResultSet, options: &ExportOptions, out: &mut impl Write) -> io::Result<()> {
    let d = Dialect::MySql;
    let cols = rs
        .columns
        .iter()
        .map(|c| d.quote_ident(&c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let table = d.quote_ident(&options.table);
    for chunk in rs.rows.chunks(options.rows_per_insert.max(1)) {
        let values = chunk
            .iter()
            .map(|row| {
                let vals: Vec<String> = row.iter().map(|v| d.literal(v)).collect();
                format!("({})", vals.join(", "))
            })
            .collect::<Vec<_>>()
            .join(",\n  ");
        writeln!(out, "INSERT INTO {table} ({cols}) VALUES\n  {values};")?;
    }
    Ok(())
}

fn markdown_cell(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
}

fn markdown(rs: &ResultSet, options: &ExportOptions, out: &mut impl Write) -> io::Result<()> {
    let header: Vec<String> = rs.columns.iter().map(|c| markdown_cell(&c.name)).collect();
    writeln!(out, "| {} |", header.join(" | "))?;
    let rule: Vec<&str> = rs
        .columns
        .iter()
        .map(|c| {
            if c.type_name.contains("INT") || c.type_name.starts_with("DECIMAL") {
                "---:"
            } else {
                "---"
            }
        })
        .collect();
    writeln!(out, "| {} |", rule.join(" | "))?;
    for row in &rs.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| match v {
                Value::Null => options.null_text.clone(),
                v => markdown_cell(&full_text(v)),
            })
            .collect();
        writeln!(out, "| {} |", cells.join(" | "))?;
    }
    Ok(())
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html(rs: &ResultSet, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "<!doctype html>\n<meta charset=\"utf-8\">\n<table>\n  <thead><tr>"
    )?;
    for c in &rs.columns {
        writeln!(out, "    <th>{}</th>", html_escape(&c.name))?;
    }
    writeln!(out, "  </tr></thead>\n  <tbody>")?;
    for row in &rs.rows {
        write!(out, "    <tr>")?;
        for v in row {
            match v {
                Value::Null => write!(out, "<td><i>NULL</i></td>")?,
                v => write!(out, "<td>{}</td>", html_escape(&full_text(v)))?,
            }
        }
        writeln!(out, "</tr>")?;
    }
    writeln!(out, "  </tbody>\n</table>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::Column;

    fn sample() -> ResultSet {
        let col = |name: &str, type_name: &str| Column {
            name: name.into(),
            type_name: type_name.into(),
            ..Column::default()
        };
        ResultSet {
            columns: vec![
                col("id", "INT"),
                col("name", "VARCHAR"),
                col("note", "TEXT"),
            ],
            rows: vec![
                vec![
                    Value::Int(1),
                    Value::Text("Ada, \"the first\"".into()),
                    Value::Null,
                ],
                vec![
                    Value::Int(2),
                    Value::Text("=SUM(A1)".into()),
                    Value::Text("a|b\nc".into()),
                ],
            ],
            truncated: false,
        }
    }

    fn render(format: ExportFormat, tweak: impl FnOnce(&mut ExportOptions)) -> String {
        let mut options = ExportOptions {
            format,
            ..ExportOptions::default()
        };
        tweak(&mut options);
        export_to_string(&sample(), &options)
    }

    #[test]
    fn csv_quotes_and_nulls() {
        assert_eq!(
            render(ExportFormat::Csv, |_| {}),
            "id,name,note\n1,\"Ada, \"\"the first\"\"\",\n2,=SUM(A1),\"a|b\nc\"\n"
        );
        let guarded = render(ExportFormat::Csv, |o| {
            o.escape_formulas = true;
            o.header = false;
            o.delimiter = ';';
            o.null_text = "NULL".into();
        });
        // Quotes inside a field always force quoting, whatever the delimiter.
        assert_eq!(
            guarded,
            "1;\"Ada, \"\"the first\"\"\";NULL\n2;'=SUM(A1);\"a|b\nc\"\n"
        );
    }

    #[test]
    fn json_keeps_types() {
        assert_eq!(
            render(ExportFormat::Json, |_| {}),
            "[\n  {\"id\": 1, \"name\": \"Ada, \\\"the first\\\"\", \"note\": null},\n  \
             {\"id\": 2, \"name\": \"=SUM(A1)\", \"note\": \"a|b\\nc\"}\n]\n"
        );
    }

    #[test]
    fn sql_batches_rows() {
        let sql = render(ExportFormat::SqlInsert, |o| {
            o.table = "people".into();
            o.rows_per_insert = 1;
        });
        assert_eq!(
            sql,
            "INSERT INTO `people` (`id`, `name`, `note`) VALUES\n  (1, 'Ada, \"the first\"', NULL);\n\
             INSERT INTO `people` (`id`, `name`, `note`) VALUES\n  (2, '=SUM(A1)', 'a|b\\nc');\n"
        );
    }

    #[test]
    fn markdown_escapes_pipes() {
        let md = render(ExportFormat::Markdown, |_| {});
        assert!(md.starts_with("| id | name | note |\n| ---: | --- | --- |\n"));
        assert!(md.contains("| 2 | =SUM(A1) | a\\|b c |"));
    }

    #[test]
    fn html_escapes() {
        let html = render(ExportFormat::Html, |_| {});
        assert!(html.contains("<td>Ada, &quot;the first&quot;</td><td><i>NULL</i></td>"));
    }
}
