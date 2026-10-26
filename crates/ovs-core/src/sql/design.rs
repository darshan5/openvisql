//! The table designer's model: a table's columns as the user edits them, and the
//! `CREATE TABLE` / `ALTER TABLE` that applies them.

use super::dialect::Dialect;
use crate::schema::{ColumnInfo, IndexInfo};

/// One column in the designer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnDef {
    pub name: String,
    /// Full SQL type, e.g. `VARCHAR(100)` or `INT UNSIGNED`.
    pub data_type: String,
    pub nullable: bool,
    /// Default as SQL, e.g. `0`, `'n/a'` or `CURRENT_TIMESTAMP`; empty for none.
    pub default_sql: String,
    pub auto_increment: bool,
    pub primary_key: bool,
    pub comment: String,
    /// The column's name in the existing table; `None` for a new column.
    pub original_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableDesign {
    pub name: String,
    pub columns: Vec<ColumnDef>,
}

/// Defaults that are expressions rather than literal values.
fn is_expression_default(value: &str) -> bool {
    let upper = value.to_ascii_uppercase();
    upper.starts_with("CURRENT_TIMESTAMP")
        || upper == "NULL"
        || upper.ends_with(')')
        || value.parse::<f64>().is_ok()
        || value.starts_with('\'')
}

impl TableDesign {
    /// Loads an existing table's design from its columns and indexes.
    pub fn from_table(name: &str, columns: &[ColumnInfo], indexes: &[IndexInfo]) -> Self {
        let primary: Vec<&String> = indexes
            .iter()
            .find(|i| i.is_primary())
            .map(|i| i.columns.iter().collect())
            .unwrap_or_default();
        let columns = columns
            .iter()
            .map(|c| {
                let extra = c.extra.to_ascii_lowercase();
                ColumnDef {
                    name: c.name.clone(),
                    data_type: c.column_type.clone(),
                    nullable: c.nullable,
                    default_sql: match &c.default {
                        None => String::new(),
                        Some(v) if is_expression_default(v) => v.clone(),
                        Some(v) => Dialect::MySql.quote_str(v),
                    },
                    auto_increment: extra.contains("auto_increment"),
                    primary_key: primary.contains(&&c.name),
                    comment: c.comment.clone(),
                    original_name: Some(c.name.clone()),
                }
            })
            .collect();
        Self {
            name: name.to_owned(),
            columns,
        }
    }

    fn primary_key(&self) -> Vec<&str> {
        self.columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.as_str())
            .collect()
    }

    /// Problems that would make the SQL fail, in plain words.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("The table needs a name.".into());
        }
        if self.columns.is_empty() {
            return Err("The table needs at least one column.".into());
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.columns {
            if c.name.trim().is_empty() {
                return Err("Every column needs a name.".into());
            }
            if c.data_type.trim().is_empty() {
                return Err(format!("Column {} needs a type.", c.name));
            }
            if !seen.insert(c.name.to_lowercase()) {
                return Err(format!("Column {} appears twice.", c.name));
            }
        }
        if self.columns.iter().filter(|c| c.auto_increment).count() > 1 {
            return Err("Only one column can be AUTO_INCREMENT.".into());
        }
        Ok(())
    }
}

/// `name TYPE [NOT NULL] [DEFAULT x] [AUTO_INCREMENT] [COMMENT '...']`.
pub fn column_sql(d: Dialect, c: &ColumnDef) -> String {
    let mut sql = format!("{} {}", d.quote_ident(&c.name), c.data_type.trim());
    sql.push_str(if c.nullable && !c.primary_key {
        " NULL"
    } else {
        " NOT NULL"
    });
    if !c.default_sql.trim().is_empty() {
        sql.push_str(" DEFAULT ");
        sql.push_str(c.default_sql.trim());
    }
    if c.auto_increment {
        sql.push_str(" AUTO_INCREMENT");
    }
    if !c.comment.is_empty() {
        sql.push_str(" COMMENT ");
        sql.push_str(&d.quote_str(&c.comment));
    }
    sql
}

/// `CREATE TABLE db.name (...)` for a new table.
pub fn create_sql(d: Dialect, db: &str, design: &TableDesign) -> String {
    let mut lines: Vec<String> = design
        .columns
        .iter()
        .map(|c| format!("  {}", column_sql(d, c)))
        .collect();
    let pk = design.primary_key();
    if !pk.is_empty() {
        let cols: Vec<String> = pk.iter().map(|c| d.quote_ident(c)).collect();
        lines.push(format!("  PRIMARY KEY ({})", cols.join(", ")));
    }
    format!(
        "CREATE TABLE {} (\n{}\n)",
        d.qualified(db, &design.name),
        lines.join(",\n")
    )
}

/// One `ALTER TABLE` turning `before` into `after`, or `None` when nothing changed. Columns
/// are matched by `original_name`, so renames keep their data.
pub fn alter_sql(
    d: Dialect,
    db: &str,
    before: &TableDesign,
    after: &TableDesign,
) -> Option<String> {
    let mut clauses = Vec::new();

    // Dropped columns first, so a new column may reuse a dropped name.
    for old in &before.columns {
        let kept = after
            .columns
            .iter()
            .any(|c| c.original_name.as_deref() == Some(old.name.as_str()));
        if !kept {
            clauses.push(format!("DROP COLUMN {}", d.quote_ident(&old.name)));
        }
    }

    let pk_changed = before.primary_key() != after.primary_key();
    // A primary key column can't lose AUTO_INCREMENT's key while the key is being replaced, so
    // drop the old key before changing columns and add the new one after.
    if pk_changed && !before.primary_key().is_empty() {
        clauses.push("DROP PRIMARY KEY".into());
    }

    let old_order: Vec<&str> = before
        .columns
        .iter()
        .filter(|old| {
            after
                .columns
                .iter()
                .any(|c| c.original_name.as_deref() == Some(old.name.as_str()))
        })
        .map(|c| c.name.as_str())
        .collect();
    let new_order: Vec<&str> = after
        .columns
        .iter()
        .filter_map(|c| c.original_name.as_deref())
        .collect();

    for (ix, col) in after.columns.iter().enumerate() {
        let position = match ix {
            0 => " FIRST".to_owned(),
            _ => format!(" AFTER {}", d.quote_ident(&after.columns[ix - 1].name)),
        };
        match &col.original_name {
            None => clauses.push(format!("ADD COLUMN {}{position}", column_sql(d, col))),
            Some(original) => {
                let Some(old) = before.columns.iter().find(|c| &c.name == original) else {
                    continue;
                };
                // Compare everything but the key flag, which is handled separately.
                let changed = ColumnDef {
                    primary_key: false,
                    ..old.clone()
                } != ColumnDef {
                    primary_key: false,
                    original_name: old.original_name.clone(),
                    ..col.clone()
                };
                let moved = old_order != new_order;
                if changed || moved {
                    let position = if moved { position } else { String::new() };
                    clauses.push(format!(
                        "CHANGE COLUMN {} {}{position}",
                        d.quote_ident(original),
                        column_sql(d, col)
                    ));
                }
            }
        }
    }

    if pk_changed && !after.primary_key().is_empty() {
        let cols: Vec<String> = after
            .primary_key()
            .iter()
            .map(|c| d.quote_ident(c))
            .collect();
        clauses.push(format!("ADD PRIMARY KEY ({})", cols.join(", ")));
    }
    if before.name != after.name {
        clauses.push(format!("RENAME TO {}", d.qualified(db, &after.name)));
    }
    if clauses.is_empty() {
        return None;
    }
    Some(format!(
        "ALTER TABLE {}\n  {}",
        d.qualified(db, &before.name),
        clauses.join(",\n  ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Dialect = Dialect::MySql;

    fn col(name: &str, data_type: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
            original_name: Some(name.into()),
            ..ColumnDef::default()
        }
    }

    fn table() -> TableDesign {
        TableDesign {
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    primary_key: true,
                    auto_increment: true,
                    nullable: false,
                    ..col("id", "INT UNSIGNED")
                },
                ColumnDef {
                    default_sql: "'n/a'".into(),
                    comment: "who".into(),
                    ..col("name", "VARCHAR(40)")
                },
            ],
        }
    }

    #[test]
    fn creates_tables() {
        assert_eq!(
            create_sql(D, "shop", &table()),
            "CREATE TABLE `shop`.`t` (\n  `id` INT UNSIGNED NOT NULL AUTO_INCREMENT,\n  \
             `name` VARCHAR(40) NULL DEFAULT 'n/a' COMMENT 'who',\n  PRIMARY KEY (`id`)\n)"
        );
    }

    #[test]
    fn unchanged_design_needs_no_alter() {
        assert_eq!(alter_sql(D, "shop", &table(), &table()), None);
    }

    #[test]
    fn alters_rename_modify_add_drop() {
        let before = table();
        let mut after = table();
        after.columns[1].name = "full_name".into();
        after.columns[1].data_type = "VARCHAR(100)".into();
        after.columns.push(ColumnDef {
            original_name: None,
            ..col("born", "DATE")
        });
        assert_eq!(
            alter_sql(D, "shop", &before, &after).unwrap(),
            "ALTER TABLE `shop`.`t`\n  CHANGE COLUMN `name` `full_name` VARCHAR(100) NULL \
             DEFAULT 'n/a' COMMENT 'who',\n  ADD COLUMN `born` DATE NULL AFTER `full_name`"
        );
        let mut dropped = table();
        dropped.columns.pop();
        assert_eq!(
            alter_sql(D, "shop", &before, &dropped).unwrap(),
            "ALTER TABLE `shop`.`t`\n  DROP COLUMN `name`"
        );
    }

    #[test]
    fn reorders_and_changes_primary_key() {
        let before = table();
        let mut after = table();
        after.columns.swap(0, 1);
        after.columns[0].primary_key = true;
        after.columns[0].nullable = false;
        let sql = alter_sql(D, "shop", &before, &after).unwrap();
        assert!(sql.contains("DROP PRIMARY KEY"), "{sql}");
        assert!(
            sql.contains("CHANGE COLUMN `name` `name` VARCHAR(40) NOT NULL"),
            "{sql}"
        );
        assert!(sql.contains(" FIRST"), "{sql}");
        assert!(sql.ends_with("ADD PRIMARY KEY (`name`, `id`)"), "{sql}");
    }

    #[test]
    fn loads_existing_tables() {
        let columns = [
            ColumnInfo {
                name: "id".into(),
                column_type: "int(10) unsigned".into(),
                key: "PRI".into(),
                extra: "auto_increment".into(),
                ..ColumnInfo::default()
            },
            ColumnInfo {
                name: "status".into(),
                column_type: "varchar(10)".into(),
                nullable: true,
                default: Some("new".into()),
                ..ColumnInfo::default()
            },
            ColumnInfo {
                name: "at".into(),
                column_type: "datetime".into(),
                default: Some("CURRENT_TIMESTAMP".into()),
                ..ColumnInfo::default()
            },
        ];
        let indexes = [IndexInfo {
            name: "PRIMARY".into(),
            columns: vec!["id".into()],
            unique: true,
            index_type: "BTREE".into(),
        }];
        let design = TableDesign::from_table("t", &columns, &indexes);
        assert!(design.columns[0].primary_key && design.columns[0].auto_increment);
        assert_eq!(design.columns[1].default_sql, "'new'");
        assert_eq!(design.columns[2].default_sql, "CURRENT_TIMESTAMP");
        assert!(design.validate().is_ok());
    }

    #[test]
    fn validation_catches_mistakes() {
        let mut t = table();
        t.columns[1].name = "ID".into();
        assert!(t.validate().unwrap_err().contains("twice"));
        t.columns[1].name = String::new();
        assert!(t.validate().is_err());
    }
}
