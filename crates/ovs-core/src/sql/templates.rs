//! SQL text generated for object-browser commands: `CREATE` templates for new objects,
//! statement templates for a table, and info queries.

use super::dialect::Dialect;
use crate::schema::{ColumnInfo, ObjectKind};

const D: Dialect = Dialect::MySql;

/// A starting point for creating a new object of `kind` in `db`, opened in the editor for the
/// user to adjust. `table` is needed for triggers.
pub fn create_template(d: Dialect, kind: ObjectKind, db: &str, table: Option<&str>) -> String {
    if d == Dialect::Sqlite {
        return sqlite_create_template(kind, db, table);
    }
    let q = |name: &str| D.qualified(db, name);
    match kind {
        ObjectKind::Database => "CREATE DATABASE `new_database`\n  CHARACTER SET utf8mb4\n  COLLATE utf8mb4_unicode_ci;\n".to_owned(),
        ObjectKind::Table => format!(
            "CREATE TABLE {} (\n  `id` INT UNSIGNED NOT NULL AUTO_INCREMENT,\n  `name` VARCHAR(100) NOT NULL,\n  `created_at` DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;\n",
            q("new_table")
        ),
        ObjectKind::View => format!(
            "CREATE VIEW {} AS\nSELECT\n  1 AS `example`;\n",
            q("new_view")
        ),
        ObjectKind::Procedure => format!(
            "DELIMITER $$\nCREATE PROCEDURE {}(IN p_id INT)\nBEGIN\n  SELECT p_id;\nEND$$\nDELIMITER ;\n",
            q("new_procedure")
        ),
        ObjectKind::Function => format!(
            "DELIMITER $$\nCREATE FUNCTION {}(p_value INT)\nRETURNS INT\nDETERMINISTIC\nBEGIN\n  RETURN p_value;\nEND$$\nDELIMITER ;\n",
            q("new_function")
        ),
        ObjectKind::Trigger => format!(
            "DELIMITER $$\nCREATE TRIGGER {}\nBEFORE INSERT ON {}\nFOR EACH ROW\nBEGIN\n  -- SET NEW.`column` = ...;\nEND$$\nDELIMITER ;\n",
            q("new_trigger"),
            q(table.unwrap_or("table_name"))
        ),
        ObjectKind::Event => format!(
            "CREATE EVENT {}\nON SCHEDULE EVERY 1 DAY\nDO\n  SELECT 1;\n",
            q("new_event")
        ),
    }
}

fn sqlite_create_template(kind: ObjectKind, db: &str, table: Option<&str>) -> String {
    let d = Dialect::Sqlite;
    let q = |name: &str| d.qualified(db, name);
    match kind {
        ObjectKind::Table => format!(
            "CREATE TABLE {} (\n  \"id\" INTEGER PRIMARY KEY,\n  \"name\" TEXT NOT NULL,\n  \"created_at\" TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\n);\n",
            q("new_table")
        ),
        ObjectKind::View => format!(
            "CREATE VIEW {} AS\nSELECT\n  1 AS \"example\";\n",
            q("new_view")
        ),
        // The trigger's table is named without a schema: it must be in the trigger's own.
        ObjectKind::Trigger => format!(
            "CREATE TRIGGER {}\nAFTER INSERT ON {}\nFOR EACH ROW\nBEGIN\n  SELECT NEW.rowid;\nEND;\n",
            q("new_trigger"),
            d.quote_ident(table.unwrap_or("table_name"))
        ),
        ObjectKind::Database | ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Event => {
            format!("-- SQLite has no {}s.\n", kind.sql_keyword().to_lowercase())
        }
    }
}

/// Kinds of statement that can be pasted for a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementTemplate {
    Select,
    Insert,
    Update,
    Delete,
}

impl StatementTemplate {
    pub const ALL: [Self; 4] = [Self::Select, Self::Insert, Self::Update, Self::Delete];

    pub fn label(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

/// A statement for `db.table` listing its columns, with `?` placeholders for values. UPDATE
/// and DELETE match on the primary key (or every column when there is none).
pub fn statement_template(
    d: Dialect,
    template: StatementTemplate,
    db: &str,
    table: &str,
    columns: &[ColumnInfo],
    row_limit: usize,
) -> String {
    let target = d.qualified(db, table);
    let names: Vec<String> = columns.iter().map(|c| d.quote_ident(&c.name)).collect();
    let keys: Vec<&ColumnInfo> = {
        let pk: Vec<&ColumnInfo> = columns.iter().filter(|c| c.is_primary()).collect();
        if pk.is_empty() {
            columns.iter().collect()
        } else {
            pk
        }
    };
    let where_clause = keys
        .iter()
        .map(|c| format!("{} = ?", d.quote_ident(&c.name)))
        .collect::<Vec<_>>()
        .join(" AND ");
    match template {
        StatementTemplate::Select => format!(
            "SELECT {}\nFROM {target}\nLIMIT {row_limit};",
            names.join(", ")
        ),
        StatementTemplate::Insert => format!(
            "INSERT INTO {target}\n  ({})\nVALUES\n  ({});",
            names.join(", "),
            vec!["?"; names.len()].join(", ")
        ),
        StatementTemplate::Update => {
            let set = columns
                .iter()
                .filter(|c| !c.is_primary())
                .map(|c| format!("  {} = ?", d.quote_ident(&c.name)))
                .collect::<Vec<_>>()
                .join(",\n");
            format!("UPDATE {target}\nSET\n{set}\nWHERE {where_clause};")
        }
        StatementTemplate::Delete => format!("DELETE FROM {target}\nWHERE {where_clause};"),
    }
}

/// Statements describing a table: status, columns, indexes and DDL, one result each.
pub fn table_info(d: Dialect, db: &str, table: &str) -> String {
    if d == Dialect::Sqlite {
        let schema = d.quote_ident(db);
        let name = d.quote_str(table);
        return format!(
            "PRAGMA {schema}.table_xinfo({name});\nPRAGMA {schema}.index_list({name});\nPRAGMA {schema}.foreign_key_list({name});\nSELECT sql FROM {schema}.sqlite_master WHERE tbl_name = {name};\n"
        );
    }
    let target = D.qualified(db, table);
    format!(
        "SHOW TABLE STATUS FROM {} LIKE {};\nSHOW FULL COLUMNS FROM {target};\nSHOW INDEX FROM {target};\nSHOW CREATE TABLE {target};\n",
        D.quote_ident(db),
        D.quote_str(table)
    )
}

/// Statements describing a database: its tables with sizes, and its DDL.
pub fn database_info(d: Dialect, db: &str) -> String {
    if d == Dialect::Sqlite {
        let schema = d.quote_ident(db);
        return format!(
            "SELECT type, name, tbl_name FROM {schema}.sqlite_master ORDER BY type, name;\nSELECT * FROM pragma_database_list WHERE name = {};\n",
            d.quote_str(db)
        );
    }
    format!(
        "SELECT TABLE_NAME AS `table`, ENGINE AS `engine`, TABLE_ROWS AS `rows (approx.)`,\n  ROUND((DATA_LENGTH + INDEX_LENGTH) / 1024 / 1024, 2) AS `size (MB)`,\n  TABLE_COLLATION AS `collation`, TABLE_COMMENT AS `comment`\nFROM information_schema.TABLES\nWHERE TABLE_SCHEMA = {}\nORDER BY TABLE_NAME;\nSHOW CREATE DATABASE {};\n",
        D.quote_str(db),
        D.quote_ident(db)
    )
}

/// Running connections, longest-running first.
pub const PROCESS_LIST: &str = "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, LEFT(INFO, 500) AS INFO\nFROM information_schema.PROCESSLIST\nORDER BY TIME DESC;\n";

/// Version, settings and key counters of the server.
pub const SERVER_INFO: &str = "SELECT VERSION() AS `version`, @@version_comment AS `distribution`,\n  @@hostname AS `host`, @@port AS `port`, @@character_set_server AS `charset`,\n  @@collation_server AS `collation`, @@time_zone AS `time zone`, @@datadir AS `data dir`;\nSHOW GLOBAL STATUS WHERE Variable_name IN\n  ('Uptime', 'Threads_connected', 'Threads_running', 'Questions', 'Slow_queries',\n   'Connections', 'Aborted_connects', 'Bytes_received', 'Bytes_sent');\n";

pub const STATUS_VARIABLES: &str = "SHOW GLOBAL STATUS;\n";
pub const SYSTEM_VARIABLES: &str = "SHOW GLOBAL VARIABLES;\n";

/// True for a statement that lists server processes, whose rows can be killed.
pub fn is_process_list(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    upper.contains("INFORMATION_SCHEMA.PROCESSLIST")
        || upper.contains("PROCESSLIST") && upper.trim_start().starts_with("SHOW")
}

/// `KILL QUERY id` stops the running statement; `KILL id` closes the whole connection.
pub fn kill(id: u64, connection: bool) -> String {
    if connection {
        format!("KILL {id}")
    } else {
        format!("KILL QUERY {id}")
    }
}

/// `DROP <kind> db.name`; an attached SQLite database is detached instead.
pub fn drop_object(d: Dialect, kind: ObjectKind, db: &str, name: &str) -> String {
    match (d, kind) {
        (Dialect::Sqlite, ObjectKind::Database) => {
            format!("DETACH DATABASE {}", d.quote_ident(name))
        }
        (_, ObjectKind::Database) => format!("DROP DATABASE {}", d.quote_ident(name)),
        _ => format!("DROP {} {}", kind.sql_keyword(), d.qualified(db, name)),
    }
}

pub fn truncate_table(d: Dialect, db: &str, table: &str) -> String {
    match d {
        Dialect::MySql => format!("TRUNCATE TABLE {}", d.qualified(db, table)),
        Dialect::Sqlite => format!("DELETE FROM {}", d.qualified(db, table)),
    }
}

pub fn rename_table(d: Dialect, db: &str, from: &str, to: &str) -> String {
    match d {
        Dialect::MySql => format!(
            "RENAME TABLE {} TO {}",
            d.qualified(db, from),
            d.qualified(db, to)
        ),
        Dialect::Sqlite => format!(
            "ALTER TABLE {} RENAME TO {}",
            d.qualified(db, from),
            d.quote_ident(to)
        ),
    }
}

pub fn create_database(name: &str) -> String {
    format!(
        "CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
        D.quote_ident(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::split_statements;

    fn col(name: &str, key: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            key: key.into(),
            ..ColumnInfo::default()
        }
    }

    #[test]
    fn statement_templates_use_columns_and_key() {
        let cols = [col("id", "PRI"), col("name", ""), col("note", "")];
        assert_eq!(
            statement_template(D, StatementTemplate::Select, "shop", "t", &cols, 50),
            "SELECT `id`, `name`, `note`\nFROM `shop`.`t`\nLIMIT 50;"
        );
        assert_eq!(
            statement_template(D, StatementTemplate::Insert, "shop", "t", &cols, 50),
            "INSERT INTO `shop`.`t`\n  (`id`, `name`, `note`)\nVALUES\n  (?, ?, ?);"
        );
        assert_eq!(
            statement_template(D, StatementTemplate::Update, "shop", "t", &cols, 50),
            "UPDATE `shop`.`t`\nSET\n  `name` = ?,\n  `note` = ?\nWHERE `id` = ?;"
        );
        let keyless = [col("a", ""), col("b", "")];
        assert_eq!(
            statement_template(D, StatementTemplate::Delete, "shop", "t", &keyless, 50),
            "DELETE FROM `shop`.`t`\nWHERE `a` = ? AND `b` = ?;"
        );
    }

    #[test]
    fn create_templates_split_into_one_statement() {
        for kind in [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Procedure,
            ObjectKind::Function,
            ObjectKind::Trigger,
            ObjectKind::Event,
        ] {
            for d in [Dialect::MySql, Dialect::Sqlite] {
                let sql = create_template(d, kind, "shop", Some("orders"));
                assert!(split_statements(&sql).len() <= 1, "{kind:?}: {sql}");
            }
        }
    }

    #[test]
    fn process_list_detection_and_kill() {
        assert!(is_process_list(PROCESS_LIST));
        assert!(is_process_list("show full processlist"));
        assert!(!is_process_list("select * from orders"));
        assert_eq!(kill(42, false), "KILL QUERY 42");
        assert_eq!(kill(42, true), "KILL 42");
        assert_eq!(split_statements(SERVER_INFO).len(), 2);
    }

    #[test]
    fn ddl_helpers_quote_names() {
        assert_eq!(
            drop_object(D, ObjectKind::Table, "shop", "o`rders"),
            "DROP TABLE `shop`.`o``rders`"
        );
        assert_eq!(
            drop_object(D, ObjectKind::Database, "", "shop"),
            "DROP DATABASE `shop`"
        );
        assert_eq!(
            rename_table(D, "shop", "a", "b"),
            "RENAME TABLE `shop`.`a` TO `shop`.`b`"
        );
        assert_eq!(split_statements(&table_info(D, "shop", "t")).len(), 4);
        assert_eq!(split_statements(&database_info(D, "shop")).len(), 2);
        let s = Dialect::Sqlite;
        assert_eq!(split_statements(&table_info(s, "main", "t")).len(), 4);
        assert_eq!(
            rename_table(s, "main", "a", "b"),
            "ALTER TABLE \"main\".\"a\" RENAME TO \"b\""
        );
        assert_eq!(truncate_table(s, "main", "t"), "DELETE FROM \"main\".\"t\"");
        assert_eq!(
            drop_object(s, ObjectKind::Database, "", "aux"),
            "DETACH DATABASE \"aux\""
        );
    }
}
