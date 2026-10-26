//! Editing a result set in the grid: deciding whether (and how) rows can be written back, and
//! collecting pending changes until the user saves.
//!
//! A result is editable when its columns come from a single table. Rows are located on save by
//! the best identifier available, in this order:
//!
//! 1. the primary key,
//! 2. a unique index whose columns are all `NOT NULL`,
//! 3. otherwise every comparable column of the row, with `LIMIT 1` so at most one row changes.
//!
//! A key is only used when all of its columns are present in the result.

use std::collections::{BTreeMap, BTreeSet};

use crate::result::ResultSet;
use crate::schema::{ColumnInfo, IndexInfo};
use crate::sql::{CellEdit, Dialect, RowEdit, RowKey, generate_edit};
use crate::value::Value;

/// How rows of an editable result are written back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTarget {
    pub schema: String,
    pub table: String,
    /// For each result column, the table column it maps to, or `None` for expressions and
    /// columns from other tables (which stay read-only).
    pub columns: Vec<Option<String>>,
    pub key: KeyPlan,
}

/// Result-column indices used to locate a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPlan {
    Unique(Vec<usize>),
    /// No usable key: match all of these columns and touch at most one row.
    AllColumns(Vec<usize>),
}

/// The single `(schema, table)` every non-expression column comes from, if there is one.
pub fn single_source_table(rs: &ResultSet) -> Option<(String, String)> {
    let mut tables = source_tables(rs);
    if tables.len() == 1 {
        tables.pop()
    } else {
        None
    }
}

/// Every distinct `(schema, table)` the result's columns come from, in column order.
pub fn source_tables(rs: &ResultSet) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for col in rs.columns.iter().filter(|c| !c.table.is_empty()) {
        if !out.iter().any(|(s, t)| *s == col.schema && *t == col.table) {
            out.push((col.schema.clone(), col.table.clone()));
        }
    }
    out
}

impl EditTarget {
    /// Works out how `rs` can be edited, given the source table's columns and indexes.
    /// Returns `None` when the result can't be edited at all.
    pub fn detect(
        rs: &ResultSet,
        table_columns: &[ColumnInfo],
        indexes: &[IndexInfo],
    ) -> Option<Self> {
        let (schema, table) = single_source_table(rs)?;
        Self::detect_for(rs, &schema, &table, table_columns, indexes)
    }

    /// Like [`Self::detect`], editing only the columns of `schema.table`; the result may come
    /// from several tables (a join). Columns of other tables stay read-only.
    pub fn detect_for(
        rs: &ResultSet,
        schema: &str,
        table: &str,
        table_columns: &[ColumnInfo],
        indexes: &[IndexInfo],
    ) -> Option<Self> {
        let (schema, table) = (schema.to_owned(), table.to_owned());
        let columns: Vec<Option<String>> = rs
            .columns
            .iter()
            .map(|c| {
                (c.table == table && c.schema == schema && !c.origin_name.is_empty())
                    .then(|| c.origin_name.clone())
            })
            .collect();
        if columns.iter().all(Option::is_none) {
            return None;
        }
        let position = |name: &str| {
            columns
                .iter()
                .position(|c| c.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(name)))
        };
        let not_null = |name: &str| {
            table_columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(name) && !c.nullable)
        };
        let usable = |idx: &IndexInfo| -> Option<Vec<usize>> {
            idx.columns.iter().map(|c| position(c)).collect()
        };

        let key =
            indexes
                .iter()
                .filter(|i| i.is_primary())
                .chain(indexes.iter().filter(|i| {
                    i.unique && !i.is_primary() && i.columns.iter().all(|c| not_null(c))
                }))
                .find_map(usable)
                .map_or_else(
                    || {
                        // Floats don't round-trip exactly through text, so they can't be matched on.
                        let comparable = rs
                            .columns
                            .iter()
                            .enumerate()
                            .filter(|(ix, c)| {
                                columns[*ix].is_some()
                                    && !matches!(
                                        c.type_name.split(' ').next(),
                                        Some("FLOAT" | "DOUBLE")
                                    )
                            })
                            .map(|(ix, _)| ix)
                            .collect();
                        KeyPlan::AllColumns(comparable)
                    },
                    KeyPlan::Unique,
                );
        Some(Self {
            schema,
            table,
            columns,
            key,
        })
    }

    pub fn is_column_editable(&self, col: usize) -> bool {
        self.columns.get(col).is_some_and(Option::is_some)
    }

    /// True when rows are located by a real key rather than by matching every column.
    pub fn has_unique_key(&self) -> bool {
        matches!(self.key, KeyPlan::Unique(_))
    }

    fn row_key(&self, row: &[Value]) -> RowKey {
        let pairs = |cols: &[usize]| {
            cols.iter()
                .filter_map(|&ix| Some((self.columns[ix].clone()?, row.get(ix)?.clone())))
                .collect()
        };
        match &self.key {
            KeyPlan::Unique(cols) => RowKey::Unique(pairs(cols)),
            KeyPlan::AllColumns(cols) => RowKey::AllColumns(pairs(cols)),
        }
    }
}

/// Changes made in the grid that haven't been saved yet. Rows are identified by their index in
/// the original result set; inserted rows live separately.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PendingEdits {
    updates: BTreeMap<usize, BTreeMap<usize, Value>>,
    deletes: BTreeSet<usize>,
    /// New rows; `None` cells are left out of the INSERT so the server default applies.
    inserts: Vec<Vec<Option<Value>>>,
}

impl PendingEdits {
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.deletes.is_empty() && self.inserts.is_empty()
    }

    /// Number of rows that will be written.
    pub fn row_count(&self) -> usize {
        self.updates
            .keys()
            .filter(|r| !self.deletes.contains(r))
            .count()
            + self.deletes.len()
            + self.inserts.len()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Records a new value for an existing row. Setting a cell back to its original value
    /// removes the pending change.
    pub fn set_cell(&mut self, row: usize, col: usize, value: Value, original: &Value) {
        if &value == original {
            if let Some(cells) = self.updates.get_mut(&row) {
                cells.remove(&col);
                if cells.is_empty() {
                    self.updates.remove(&row);
                }
            }
        } else {
            self.updates.entry(row).or_default().insert(col, value);
        }
    }

    pub fn changed_value(&self, row: usize, col: usize) -> Option<&Value> {
        self.updates.get(&row)?.get(&col)
    }

    pub fn toggle_delete(&mut self, row: usize) {
        if !self.deletes.remove(&row) {
            self.deletes.insert(row);
        }
    }

    pub fn is_deleted(&self, row: usize) -> bool {
        self.deletes.contains(&row)
    }

    /// Adds a row pre-filled with `values` (e.g. a duplicated row) and returns its index among
    /// inserted rows.
    pub fn add_row_with(&mut self, values: Vec<Option<Value>>) -> usize {
        self.inserts.push(values);
        self.inserts.len() - 1
    }

    /// Adds an empty row and returns its index among inserted rows.
    pub fn add_row(&mut self, width: usize) -> usize {
        self.inserts.push(vec![None; width]);
        self.inserts.len() - 1
    }

    pub fn inserted_rows(&self) -> &[Vec<Option<Value>>] {
        &self.inserts
    }

    pub fn set_inserted_cell(&mut self, row: usize, col: usize, value: Option<Value>) {
        if let Some(cell) = self.inserts.get_mut(row).and_then(|r| r.get_mut(col)) {
            *cell = value;
        }
    }

    pub fn remove_inserted_row(&mut self, row: usize) {
        if row < self.inserts.len() {
            self.inserts.remove(row);
        }
    }

    /// The DML that applies every pending change, deletes first so a re-inserted key doesn't
    /// collide with the row it replaces.
    pub fn statements(
        &self,
        target: &EditTarget,
        data: &ResultSet,
        dialect: Dialect,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let generate = |edit: RowEdit| generate_edit(dialect, &target.schema, &target.table, &edit);
        for &row in &self.deletes {
            if let Some(values) = data.rows.get(row) {
                out.push(generate(RowEdit::Delete {
                    key: target.row_key(values),
                }));
            }
        }
        for (&row, cells) in &self.updates {
            if self.deletes.contains(&row) {
                continue;
            }
            let Some(values) = data.rows.get(row) else {
                continue;
            };
            let changes: Vec<CellEdit> = cells
                .iter()
                .filter_map(|(&col, value)| {
                    Some(CellEdit {
                        column: target.columns.get(col)?.clone()?,
                        value: value.clone(),
                    })
                })
                .collect();
            if !changes.is_empty() {
                out.push(generate(RowEdit::Update {
                    key: target.row_key(values),
                    changes,
                }));
            }
        }
        for row in &self.inserts {
            let values: Vec<CellEdit> = row
                .iter()
                .enumerate()
                .filter_map(|(col, value)| {
                    Some(CellEdit {
                        column: target.columns.get(col)?.clone()?,
                        value: value.clone()?,
                    })
                })
                .collect();
            if !values.is_empty() {
                out.push(generate(RowEdit::Insert { values }));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::Column;

    fn col(name: &str, table: &str, type_name: &str) -> Column {
        Column {
            name: name.into(),
            origin_name: name.into(),
            table: table.into(),
            schema: "shop".into(),
            type_name: type_name.into(),
            ..Column::default()
        }
    }

    fn info(name: &str, nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            nullable,
            ..ColumnInfo::default()
        }
    }

    fn index(name: &str, cols: &[&str], unique: bool) -> IndexInfo {
        IndexInfo {
            name: name.into(),
            columns: cols.iter().map(|c| (*c).to_owned()).collect(),
            unique,
            index_type: "BTREE".into(),
            prefix_lengths: Vec::new(),
            constraint_name: String::new(),
        }
    }

    fn orders() -> ResultSet {
        ResultSet {
            columns: vec![
                col("id", "orders", "INT UNSIGNED"),
                col("status", "orders", "ENUM"),
                Column {
                    name: "total".into(),
                    ..Column::default()
                },
            ],
            rows: vec![
                vec![Value::UInt(1), Value::Text("new".into()), Value::Int(5)],
                vec![Value::UInt(2), Value::Text("paid".into()), Value::Int(7)],
            ],
            truncated: false,
        }
    }

    #[test]
    fn detects_primary_key_and_read_only_expressions() {
        let target = EditTarget::detect(
            &orders(),
            &[info("id", false), info("status", false)],
            &[index("PRIMARY", &["id"], true)],
        )
        .unwrap();
        assert_eq!(target.table, "orders");
        assert_eq!(target.key, KeyPlan::Unique(vec![0]));
        assert!(target.is_column_editable(1));
        assert!(!target.is_column_editable(2), "computed column");
    }

    #[test]
    fn falls_back_to_not_null_unique_then_all_columns() {
        let mut rs = orders();
        rs.columns[0].origin_name = "id".into();
        // Primary key column not selected; unique email-like column is.
        rs.columns[0] = col("code", "orders", "VARCHAR");
        let indexes = [
            index("PRIMARY", &["id"], true),
            index("uq_code", &["code"], true),
        ];
        let target = EditTarget::detect(&rs, &[info("code", false)], &indexes).unwrap();
        assert_eq!(target.key, KeyPlan::Unique(vec![0]));

        // Nullable unique columns can't identify a row.
        let target = EditTarget::detect(&rs, &[info("code", true)], &indexes).unwrap();
        assert_eq!(target.key, KeyPlan::AllColumns(vec![0, 1]));
    }

    #[test]
    fn joins_can_edit_one_chosen_table() {
        let mut rs = orders();
        rs.columns.push(col("name", "customers", "VARCHAR"));
        for row in &mut rs.rows {
            row.push(Value::Text("x".into()));
        }
        assert_eq!(
            source_tables(&rs),
            [
                ("shop".to_owned(), "orders".to_owned()),
                ("shop".to_owned(), "customers".to_owned())
            ]
        );
        let target = EditTarget::detect_for(
            &rs,
            "shop",
            "orders",
            &[],
            &[index("PRIMARY", &["id"], true)],
        )
        .unwrap();
        assert!(target.is_column_editable(1));
        assert!(!target.is_column_editable(3), "customers.name is read-only");
        assert_eq!(target.key, KeyPlan::Unique(vec![0]));
    }

    #[test]
    fn rejects_multi_table_results() {
        let mut rs = orders();
        rs.columns[1].table = "customers".into();
        assert_eq!(single_source_table(&rs), None);
        assert!(EditTarget::detect(&rs, &[], &[]).is_none());
    }

    #[test]
    fn pending_edits_generate_dml() {
        let rs = orders();
        let target = EditTarget::detect(&rs, &[], &[index("PRIMARY", &["id"], true)]).unwrap();
        let mut edits = PendingEdits::default();
        edits.set_cell(0, 1, Value::Text("shipped".into()), &rs.rows[0][1]);
        edits.toggle_delete(1);
        let new = edits.add_row(3);
        edits.set_inserted_cell(new, 1, Some(Value::Text("new".into())));
        edits.set_inserted_cell(new, 2, Some(Value::Int(9))); // computed column: ignored
        assert_eq!(edits.row_count(), 3);
        assert_eq!(
            edits.statements(&target, &rs, Dialect::MySql),
            [
                "DELETE FROM `shop`.`orders` WHERE `id` = 2",
                "UPDATE `shop`.`orders` SET `status` = 'shipped' WHERE `id` = 1",
                "INSERT INTO `shop`.`orders` (`status`) VALUES ('new')",
            ]
        );
    }

    #[test]
    fn reverting_a_cell_clears_the_change() {
        let rs = orders();
        let mut edits = PendingEdits::default();
        edits.set_cell(0, 1, Value::Text("x".into()), &rs.rows[0][1]);
        assert!(!edits.is_empty());
        edits.set_cell(0, 1, Value::Text("new".into()), &rs.rows[0][1]);
        assert!(edits.is_empty());
    }

    #[test]
    fn keyless_rows_skip_floats() {
        let rs = ResultSet {
            columns: vec![
                col("msg", "audit_log", "VARCHAR"),
                col("ratio", "audit_log", "DOUBLE"),
            ],
            rows: vec![vec![Value::Text("a".into()), Value::Float(0.1)]],
            truncated: false,
        };
        let target = EditTarget::detect(&rs, &[], &[]).unwrap();
        assert_eq!(target.key, KeyPlan::AllColumns(vec![0]));
        let mut edits = PendingEdits::default();
        edits.toggle_delete(0);
        assert_eq!(
            edits.statements(&target, &rs, Dialect::MySql),
            ["DELETE FROM `shop`.`audit_log` WHERE `msg` = 'a' LIMIT 1"]
        );
    }
}
