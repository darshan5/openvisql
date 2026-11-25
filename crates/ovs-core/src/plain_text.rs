//! Plain-text renderings of a result set, for reading at a glance, in a terminal, or through a
//! screenshot of a remote screen:
//!
//! - [`text_table`]: a boxed table, like the `mysql` command-line client prints.
//! - [`records`]: one block per row with `column: value` lines, like the client's `\G`.

use std::fmt::Write as _;

use crate::result::ResultSet;
use crate::value::Value;

/// How a value reads in plain text. NULL is spelled out and binary values are shown as hex, so
/// nothing is invisible.
fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Bytes(b) => format!("0x{}", crate::value::hex(b)),
        v => v.to_string(),
    }
}

/// `text` cut to `max` characters (with `…` at the end when cut). `max == 0` means no limit.
fn clip(text: &str, max: usize) -> String {
    if max == 0 || text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A value on one line: line breaks and tabs become spaces so the table's columns stay aligned.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The rows to print, in `order` when given (e.g. the grid's sorted order), else as fetched.
fn rows<'a>(
    rs: &'a ResultSet,
    order: Option<&'a [usize]>,
) -> Box<dyn Iterator<Item = &'a Vec<Value>> + 'a> {
    match order {
        Some(order) => Box::new(order.iter().filter_map(|&i| rs.rows.get(i))),
        None => Box::new(rs.rows.iter()),
    }
}

/// A boxed table:
///
/// ```text
/// +---------+-------------+
/// | id      | order_id    |
/// +---------+-------------+
/// |    1042 |       20871 |
/// +---------+-------------+
/// ```
///
/// Numbers are right-aligned. Cells longer than `max_cell_chars` are cut (`0` = never).
pub fn text_table(rs: &ResultSet, order: Option<&[usize]>, max_cell_chars: usize) -> String {
    let headers: Vec<String> = rs
        .columns
        .iter()
        .map(|c| clip(&one_line(&c.name), max_cell_chars))
        .collect();
    let body: Vec<Vec<(String, bool)>> = rows(rs, order)
        .map(|row| {
            row.iter()
                .map(|v| {
                    let numeric = matches!(v, Value::Int(_) | Value::UInt(_) | Value::Float(_));
                    (clip(&one_line(&cell_text(v)), max_cell_chars), numeric)
                })
                .collect()
        })
        .collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &body {
        for (ix, (text, _)) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(ix) {
                *w = (*w).max(text.chars().count());
            }
        }
    }

    let rule = {
        let mut line = String::from("+");
        for w in &widths {
            line.push_str(&"-".repeat(w + 2));
            line.push('+');
        }
        line
    };
    let pad = |text: &str, width: usize, right: bool| {
        let fill = " ".repeat(width.saturating_sub(text.chars().count()));
        if right {
            format!("{fill}{text}")
        } else {
            format!("{text}{fill}")
        }
    };

    let mut out = String::new();
    out.push_str(&rule);
    out.push('\n');
    out.push('|');
    for (h, w) in headers.iter().zip(&widths) {
        let _ = write!(out, " {} |", pad(h, *w, false));
    }
    out.push('\n');
    out.push_str(&rule);
    out.push('\n');
    for row in &body {
        out.push('|');
        for (ix, w) in widths.iter().enumerate() {
            let (text, numeric) = row.get(ix).map_or(("", false), |(t, n)| (t.as_str(), *n));
            let _ = write!(out, " {} |", pad(text, *w, numeric));
        }
        out.push('\n');
    }
    if !body.is_empty() {
        out.push_str(&rule);
        out.push('\n');
    }
    out
}

/// One block per row, column names right-aligned:
///
/// ```text
/// *************************** 1. row ***************************
///       id: 1042
/// order_id: 20871
/// ```
///
/// Multi-line values keep their line breaks, indented under the value. Values longer than
/// `max_cell_chars` are cut (`0` = never).
pub fn records(rs: &ResultSet, order: Option<&[usize]>, max_cell_chars: usize) -> String {
    let width = rs
        .columns
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    let indent = " ".repeat(width + 2);
    let mut out = String::new();
    for (n, row) in rows(rs, order).enumerate() {
        let _ = writeln!(
            out,
            "*************************** {}. row ***************************",
            n + 1
        );
        for (col, value) in rs.columns.iter().zip(row) {
            let name = format!("{:>width$}", col.name);
            let text = clip(&cell_text(value), max_cell_chars);
            let mut lines = text.lines();
            let first = lines.next().unwrap_or_default();
            let _ = writeln!(out, "{name}: {first}");
            for more in lines {
                let _ = writeln!(out, "{indent}{more}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::Column;

    fn rs() -> ResultSet {
        let col = |name: &str| Column {
            name: name.into(),
            ..Column::default()
        };
        ResultSet {
            columns: vec![col("id"), col("name"), col("note")],
            rows: vec![
                vec![Value::Int(1), Value::Text("Cash".into()), Value::Null],
                vec![
                    Value::Int(22),
                    Value::Text("two\nlines".into()),
                    Value::Bytes(vec![0xAB]),
                ],
            ],
            truncated: false,
        }
    }

    #[test]
    fn boxed_table_aligns_and_shows_null() {
        let expected = "\
+----+-----------+------+
| id | name      | note |
+----+-----------+------+
|  1 | Cash      | NULL |
| 22 | two lines | 0xAB |
+----+-----------+------+
";
        assert_eq!(text_table(&rs(), None, 0), expected);
    }

    #[test]
    fn boxed_table_follows_order_and_clips() {
        let out = text_table(&rs(), Some(&[1, 0]), 4);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[3], "| 22 | two… | 0xAB |");
        assert_eq!(lines[4], "|  1 | Cash | NULL |");
    }

    #[test]
    fn empty_result_has_only_the_header() {
        let mut empty = rs();
        empty.rows.clear();
        assert_eq!(text_table(&empty, None, 0).lines().count(), 3);
        assert_eq!(records(&empty, None, 0), "");
    }

    #[test]
    fn records_one_block_per_row() {
        let out = records(&rs(), None, 0);
        let expected = "\
*************************** 1. row ***************************
  id: 1
name: Cash
note: NULL
*************************** 2. row ***************************
  id: 22
name: two
      lines
note: 0xAB
";
        assert_eq!(out, expected);
    }
}
