//! SQL text generated for object-browser commands: `CREATE` templates for new objects,
//! statement templates for a table, and info queries.

use super::dialect::Dialect;
use crate::schema::{ColumnInfo, ObjectKind};

const D: Dialect = Dialect::MySql;

/// A starting point for creating a new object of `kind` in `db`, opened in the editor for the
/// user to adjust. `table` is needed for triggers.
pub fn create_template(d: Dialect, kind: ObjectKind, db: &str, table: Option<&str>) -> String {
    match d {
        Dialect::Sqlite => return sqlite_create_template(kind, db, table),
        Dialect::Postgres => return postgres_create_template(kind, db, table),
        Dialect::MySql => {}
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
        ObjectKind::Sequence | ObjectKind::Type => {
            format!("-- MySQL has no {}s.\n", kind.sql_keyword().to_lowercase())
        }
    }
}

fn postgres_create_template(kind: ObjectKind, schema: &str, table: Option<&str>) -> String {
    let d = Dialect::Postgres;
    let q = |name: &str| d.qualified(schema, name);
    match kind {
        ObjectKind::Database => "CREATE SCHEMA \"new_schema\";\n".to_owned(),
        ObjectKind::Table => format!(
            "CREATE TABLE {} (\n  \"id\" bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,\n  \"name\" text NOT NULL,\n  \"created_at\" timestamptz NOT NULL DEFAULT now()\n);\n",
            q("new_table")
        ),
        ObjectKind::View => format!(
            "CREATE VIEW {} AS\nSELECT\n  1 AS \"example\";\n",
            q("new_view")
        ),
        ObjectKind::Procedure => format!(
            "CREATE PROCEDURE {}(p_id integer)\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  RAISE NOTICE 'id %', p_id;\nEND;\n$$;\n",
            q("new_procedure")
        ),
        ObjectKind::Function => format!(
            "CREATE FUNCTION {}(p_value integer)\nRETURNS integer\nLANGUAGE sql IMMUTABLE\nAS $$\n  SELECT p_value;\n$$;\n",
            q("new_function")
        ),
        // A trigger runs a function; both are created here.
        ObjectKind::Trigger => format!(
            "CREATE FUNCTION {func}()\nRETURNS trigger\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  -- NEW.\"column\" := ...;\n  RETURN NEW;\nEND;\n$$;\n\nCREATE TRIGGER \"new_trigger\"\nBEFORE INSERT ON {table}\nFOR EACH ROW EXECUTE FUNCTION {func}();\n",
            func = q("new_trigger_fn"),
            table = q(table.unwrap_or("table_name"))
        ),
        ObjectKind::Event => "-- PostgreSQL has no events; use the pg_cron extension.\n".to_owned(),
        ObjectKind::Sequence => format!(
            "CREATE SEQUENCE {}\n  AS bigint\n  START WITH 1\n  INCREMENT BY 1;\n",
            q("new_sequence")
        ),
        // Also: CREATE DOMAIN … AS text CHECK (…), CREATE TYPE … AS (field type, …).
        ObjectKind::Type => format!(
            "CREATE TYPE {} AS ENUM ('first', 'second');\n",
            q("new_type")
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
        ObjectKind::Database
        | ObjectKind::Procedure
        | ObjectKind::Function
        | ObjectKind::Event
        | ObjectKind::Sequence
        | ObjectKind::Type => {
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
    if d == Dialect::Postgres {
        let (schema, name) = (d.quote_str(db), d.quote_str(table));
        return format!(
            "SELECT c.relname AS \"table\", c.reltuples::bigint AS \"rows (approx.)\",\n  pg_size_pretty(pg_total_relation_size(c.oid)) AS \"total size\",\n  obj_description(c.oid, 'pg_class') AS \"comment\"\nFROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace\nWHERE n.nspname = {schema} AND c.relname = {name};\nSELECT column_name, data_type, is_nullable, column_default\nFROM information_schema.columns\nWHERE table_schema = {schema} AND table_name = {name}\nORDER BY ordinal_position;\nSELECT indexname, indexdef FROM pg_indexes\nWHERE schemaname = {schema} AND tablename = {name};\nSELECT conname, pg_get_constraintdef(oid) AS definition FROM pg_constraint\nWHERE conrelid = (quote_ident({schema}) || '.' || quote_ident({name}))::regclass;\n"
        );
    }
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
    if d == Dialect::Postgres {
        return format!(
            "SELECT c.relname AS \"name\", CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'S' THEN 'sequence' ELSE c.relkind::text END AS \"kind\",\n  c.reltuples::bigint AS \"rows (approx.)\", pg_size_pretty(pg_total_relation_size(c.oid)) AS \"size\"\nFROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace\nWHERE n.nspname = {} AND c.relkind IN ('r', 'p', 'v', 'm', 'S')\nORDER BY 2, 1;\n",
            d.quote_str(db)
        );
    }
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

/// PostgreSQL's sessions, longest-running first. The first column is the backend pid.
pub const PG_PROCESS_LIST: &str = "SELECT pid AS \"ID\", usename AS \"USER\", client_addr AS \"HOST\", datname AS \"DB\",\n  state AS \"STATE\", now() - query_start AS \"TIME\", left(query, 500) AS \"INFO\"\nFROM pg_stat_activity\nWHERE backend_type = 'client backend'\nORDER BY query_start NULLS LAST;\n";

pub const PG_SERVER_INFO: &str = "SELECT version() AS \"version\", current_database() AS \"database\",\n  current_user AS \"user\", inet_server_addr() AS \"host\", inet_server_port() AS \"port\",\n  pg_size_pretty(pg_database_size(current_database())) AS \"database size\",\n  pg_postmaster_start_time() AS \"started\";\nSELECT * FROM pg_stat_database WHERE datname = current_database();\n";

pub const PG_STATUS_VARIABLES: &str = "SELECT * FROM pg_stat_database;\n";
pub const PG_SYSTEM_VARIABLES: &str =
    "SELECT name, setting, unit, short_desc FROM pg_settings ORDER BY name;\n";

/// The process-list, server-info, status and settings queries for `d`.
pub fn server_queries(d: Dialect) -> [&'static str; 4] {
    match d {
        Dialect::Postgres => [
            PG_PROCESS_LIST,
            PG_SERVER_INFO,
            PG_STATUS_VARIABLES,
            PG_SYSTEM_VARIABLES,
        ],
        Dialect::MySql | Dialect::Sqlite => [
            PROCESS_LIST,
            SERVER_INFO,
            STATUS_VARIABLES,
            SYSTEM_VARIABLES,
        ],
    }
}

/// True for a statement that lists server processes, whose rows can be killed.
pub fn is_process_list(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    upper.contains("INFORMATION_SCHEMA.PROCESSLIST")
        || upper.contains("PG_STAT_ACTIVITY")
        || upper.contains("PROCESSLIST") && upper.trim_start().starts_with("SHOW")
}

/// Stops a session's running statement, or (`connection`) closes the session.
pub fn kill(d: Dialect, id: u64, connection: bool) -> String {
    match (d, connection) {
        (Dialect::Postgres, true) => format!("SELECT pg_terminate_backend({id})"),
        (Dialect::Postgres, false) => format!("SELECT pg_cancel_backend({id})"),
        (_, true) => format!("KILL {id}"),
        (_, false) => format!("KILL QUERY {id}"),
    }
}

/// `DROP <kind> db.name`; an attached SQLite database is detached instead.
pub fn drop_object(d: Dialect, kind: ObjectKind, db: &str, name: &str) -> String {
    match (d, kind) {
        (Dialect::Sqlite, ObjectKind::Database) => {
            format!("DETACH DATABASE {}", d.quote_ident(name))
        }
        // The object browser's PostgreSQL "databases" are schemas.
        (Dialect::Postgres, ObjectKind::Database) => {
            format!("DROP SCHEMA {}", d.quote_ident(name))
        }
        (_, ObjectKind::Database) => format!("DROP DATABASE {}", d.quote_ident(name)),
        _ => format!("DROP {} {}", kind.sql_keyword(), d.qualified(db, name)),
    }
}

/// The `DROP` for an object as the browser lists it, where `detail` (see `SchemaObject`) says
/// what `kind` alone doesn't: a trigger's table, a PostgreSQL function's arguments, whether a
/// view is materialized or a type is a domain.
pub fn drop_statement(d: Dialect, kind: ObjectKind, db: &str, name: &str, detail: &str) -> String {
    let target = d.qualified(db, name);
    match (d, kind) {
        (_, ObjectKind::Trigger) => drop_trigger(d, db, name, detail, false),
        (Dialect::Postgres, ObjectKind::View) if detail == "materialized" => {
            format!("DROP MATERIALIZED VIEW {target}")
        }
        // Overloads share a name; the arguments pick one.
        (Dialect::Postgres, ObjectKind::Procedure | ObjectKind::Function) => {
            format!("DROP {} {target}({detail})", kind.sql_keyword())
        }
        (Dialect::Postgres, ObjectKind::Type) if detail == "domain" => {
            format!("DROP DOMAIN {target}")
        }
        _ => drop_object(d, kind, db, name),
    }
}

/// Re-runs a PostgreSQL materialized view's query.
pub fn refresh_materialized_view(d: Dialect, db: &str, name: &str) -> String {
    format!("REFRESH MATERIALIZED VIEW {}", d.qualified(db, name))
}

/// `DROP TRIGGER`; PostgreSQL names triggers per table, so it needs `table` too.
pub fn drop_trigger(d: Dialect, db: &str, name: &str, table: &str, if_exists: bool) -> String {
    let exists = if if_exists { "IF EXISTS " } else { "" };
    match d {
        Dialect::Postgres => format!(
            "DROP TRIGGER {exists}{} ON {}",
            d.quote_ident(name),
            d.qualified(db, table)
        ),
        Dialect::MySql | Dialect::Sqlite => {
            format!("DROP TRIGGER {exists}{}", d.qualified(db, name))
        }
    }
}

pub fn truncate_table(d: Dialect, db: &str, table: &str) -> String {
    match d {
        Dialect::MySql | Dialect::Postgres => {
            format!("TRUNCATE TABLE {}", d.qualified(db, table))
        }
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
        Dialect::Sqlite | Dialect::Postgres => format!(
            "ALTER TABLE {} RENAME TO {}",
            d.qualified(db, from),
            d.quote_ident(to)
        ),
    }
}

/// `CREATE SCHEMA`: what "Create database" means in PostgreSQL's object browser.
pub fn create_schema(name: &str) -> String {
    format!("CREATE SCHEMA {}", Dialect::Postgres.quote_ident(name))
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
            ObjectKind::Sequence,
            ObjectKind::Type,
        ] {
            for d in [Dialect::MySql, Dialect::Sqlite] {
                let sql = create_template(d, kind, "shop", Some("orders"));
                assert!(split_statements(&sql).len() <= 1, "{kind:?}: {sql}");
            }
        }
    }

    #[test]
    fn drops_say_what_postgres_needs() {
        let p = Dialect::Postgres;
        assert_eq!(
            drop_statement(p, ObjectKind::View, "s", "mv", "materialized"),
            "DROP MATERIALIZED VIEW \"s\".\"mv\""
        );
        assert_eq!(
            drop_statement(p, ObjectKind::View, "s", "v", ""),
            "DROP VIEW \"s\".\"v\""
        );
        assert_eq!(
            drop_statement(p, ObjectKind::Function, "s", "f", "a integer, b text"),
            "DROP FUNCTION \"s\".\"f\"(a integer, b text)"
        );
        assert_eq!(
            drop_statement(p, ObjectKind::Type, "s", "email", "domain"),
            "DROP DOMAIN \"s\".\"email\""
        );
        assert_eq!(
            drop_statement(p, ObjectKind::Type, "s", "mood", "enum"),
            "DROP TYPE \"s\".\"mood\""
        );
        assert_eq!(
            drop_statement(p, ObjectKind::Trigger, "s", "t", "orders"),
            "DROP TRIGGER \"t\" ON \"s\".\"orders\""
        );
        // MySQL routines are dropped by name alone.
        assert_eq!(
            drop_statement(D, ObjectKind::Function, "shop", "f", ""),
            "DROP FUNCTION `shop`.`f`"
        );
        assert_eq!(
            refresh_materialized_view(p, "s", "mv"),
            "REFRESH MATERIALIZED VIEW \"s\".\"mv\""
        );
        for kind in [ObjectKind::Sequence, ObjectKind::Type] {
            let sql = create_template(p, kind, "s", None);
            assert_eq!(split_statements(&sql).len(), 1, "{sql}");
        }
    }

    #[test]
    fn process_list_detection_and_kill() {
        assert!(is_process_list(PROCESS_LIST));
        assert!(is_process_list("show full processlist"));
        assert!(!is_process_list("select * from orders"));
        assert_eq!(kill(D, 42, false), "KILL QUERY 42");
        assert_eq!(kill(D, 42, true), "KILL 42");
        assert_eq!(
            kill(Dialect::Postgres, 7, false),
            "SELECT pg_cancel_backend(7)"
        );
        assert!(is_process_list(PG_PROCESS_LIST));
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
