//! Catalog queries behind the object browser, and `CREATE` statements built from the catalogs
//! (PostgreSQL has no `SHOW CREATE TABLE`).

use std::fmt::Write as _;

use ovs_core::sql::Dialect;
use ovs_core::{
    CheckInfo, ColumnInfo, Error, ForeignKeyInfo, IndexInfo, ObjectKind, Result, ResultSet,
    SchemaObject, TableOptions, Value,
};

use crate::PostgresSession;

const D: Dialect = Dialect::Postgres;

/// Separates list items in one text column: a control character no name contains.
const SEP: char = '\u{1f}';

fn text(rs: &ResultSet, row: &[Value], name: &str) -> String {
    let Some(i) = rs
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))
    else {
        return String::new();
    };
    match row.get(i) {
        None | Some(Value::Null) => String::new(),
        Some(v) => v.to_string(),
    }
}

fn list(rs: &ResultSet, row: &[Value], name: &str) -> Vec<String> {
    let joined = text(rs, row, name);
    if joined.is_empty() {
        Vec::new()
    } else {
        joined.split(SEP).map(str::to_owned).collect()
    }
}

/// `'schema.table'::regclass`, with both parts quoted.
fn regclass(schema: &str, table: &str) -> String {
    format!("{}::regclass", D.quote_str(&D.qualified(schema, table)))
}

pub(crate) async fn schemas(s: &mut PostgresSession) -> Result<Vec<String>> {
    let rs = s
        .rows(
            "SELECT nspname FROM pg_namespace \
             WHERE nspname !~ '^pg_' AND nspname <> 'information_schema' ORDER BY nspname",
        )
        .await?;
    Ok(rs.rows.iter().map(|r| text(&rs, r, "nspname")).collect())
}

pub(crate) async fn objects(
    s: &mut PostgresSession,
    schema: &str,
    kind: ObjectKind,
) -> Result<Vec<SchemaObject>> {
    let ns = D.quote_str(schema);
    let sql = match kind {
        ObjectKind::Database => {
            return Ok(schemas(s)
                .await?
                .into_iter()
                .map(|name| SchemaObject {
                    name,
                    kind,
                    detail: String::new(),
                })
                .collect());
        }
        ObjectKind::Table => format!(
            "SELECT c.relname AS name, coalesce(obj_description(c.oid, 'pg_class'), '') AS detail \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {ns} AND c.relkind IN ('r', 'p') ORDER BY 1"
        ),
        ObjectKind::View => format!(
            "SELECT c.relname AS name, CASE c.relkind WHEN 'm' THEN 'materialized' ELSE '' END AS detail \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {ns} AND c.relkind IN ('v', 'm') ORDER BY 1"
        ),
        ObjectKind::Procedure | ObjectKind::Function => format!(
            "SELECT p.proname AS name, pg_get_function_identity_arguments(p.oid) AS detail \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = {ns} AND p.prokind = '{}' ORDER BY 1, 2",
            if kind == ObjectKind::Procedure {
                'p'
            } else {
                'f'
            }
        ),
        ObjectKind::Trigger => format!(
            "SELECT t.tgname AS name, c.relname AS detail \
             FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {ns} AND NOT t.tgisinternal ORDER BY 1"
        ),
        ObjectKind::Event => return Ok(Vec::new()),
    };
    let rs = s.rows(&sql).await?;
    Ok(rs
        .rows
        .iter()
        .map(|r| SchemaObject {
            name: text(&rs, r, "name"),
            kind,
            detail: text(&rs, r, "detail"),
        })
        .collect())
}

/// One row per column, shared by `columns` and the generated `CREATE TABLE`.
fn columns_query(schema: &str, table: &str) -> String {
    format!(
        "SELECT a.attname AS name, format_type(a.atttypid, a.atttypmod) AS type, \
         a.attnotnull AS notnull, a.attidentity AS identity, a.attgenerated AS generated, \
         pg_get_expr(d.adbin, d.adrelid) AS def, \
         coalesce(col_description(a.attrelid, a.attnum), '') AS comment, \
         CASE WHEN a.attcollation <> t.typcollation THEN co.collname END AS collation, \
         EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = a.attrelid AND i.indisprimary \
                 AND a.attnum = ANY(i.indkey)) AS pk \
         FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         LEFT JOIN pg_collation co ON co.oid = a.attcollation \
         WHERE a.attrelid = {} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        regclass(schema, table)
    )
}

/// A default as the catalog stores it (`'n/a'::text`) as the plain value other engines
/// report (`n/a`); expressions like `now()` are left alone.
fn plain_default(expr: &str) -> String {
    if let Some(rest) = expr.strip_prefix('\'')
        && let Some(end) = rest.rfind("'::")
    {
        return rest[..end].replace("''", "'");
    }
    expr.to_owned()
}

pub(crate) async fn columns(
    s: &mut PostgresSession,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>> {
    let rs = s.rows(&columns_query(schema, table)).await?;
    Ok(rs
        .rows
        .iter()
        .map(|r| {
            let def = text(&rs, r, "def");
            let generated = !text(&rs, r, "generated").is_empty();
            let auto = !text(&rs, r, "identity").is_empty() || def.starts_with("nextval(");
            ColumnInfo {
                name: text(&rs, r, "name"),
                column_type: text(&rs, r, "type"),
                nullable: text(&rs, r, "notnull") != "true",
                key: if text(&rs, r, "pk") == "true" {
                    "PRI".into()
                } else {
                    String::new()
                },
                default: (!def.is_empty() && !generated && !auto).then(|| plain_default(&def)),
                extra: if generated {
                    "STORED GENERATED".into()
                } else if auto {
                    "auto_increment".into()
                } else {
                    String::new()
                },
                comment: text(&rs, r, "comment"),
                collation: text(&rs, r, "collation"),
            }
        })
        .collect())
}

pub(crate) async fn indexes(
    s: &mut PostgresSession,
    schema: &str,
    table: &str,
) -> Result<Vec<IndexInfo>> {
    let sql = format!(
        "SELECT i.relname AS name, ix.indisunique AS uniq, ix.indisprimary AS pk, am.amname AS kind, \
         array_to_string(ARRAY(SELECT a.attname FROM unnest(ix.indkey) WITH ORDINALITY k(n, o) \
           JOIN pg_attribute a ON a.attrelid = ix.indrelid AND a.attnum = k.n ORDER BY k.o), \
           chr(31)) AS cols \
         FROM pg_index ix JOIN pg_class i ON i.oid = ix.indexrelid \
         JOIN pg_am am ON am.oid = i.relam \
         WHERE ix.indrelid = {} ORDER BY ix.indisprimary DESC, i.relname",
        regclass(schema, table)
    );
    let rs = s.rows(&sql).await?;
    Ok(rs
        .rows
        .iter()
        .map(|r| {
            let primary = text(&rs, r, "pk") == "true";
            IndexInfo {
                // The rest of the app knows the primary key by MySQL's name for it.
                name: if primary {
                    "PRIMARY".into()
                } else {
                    text(&rs, r, "name")
                },
                columns: list(&rs, r, "cols"),
                unique: text(&rs, r, "uniq") == "true",
                index_type: text(&rs, r, "kind").to_ascii_uppercase(),
                prefix_lengths: Vec::new(),
            }
        })
        .collect())
}

fn referential_action(code: &str) -> &'static str {
    match code {
        "r" => "RESTRICT",
        "c" => "CASCADE",
        "n" => "SET NULL",
        "d" => "SET DEFAULT",
        _ => "NO ACTION",
    }
}

pub(crate) async fn foreign_keys(
    s: &mut PostgresSession,
    schema: &str,
    table: &str,
) -> Result<Vec<ForeignKeyInfo>> {
    let names = |key: &str, rel: &str| {
        format!(
            "array_to_string(ARRAY(SELECT a.attname FROM unnest(con.{key}) WITH ORDINALITY k(n, o) \
             JOIN pg_attribute a ON a.attrelid = con.{rel} AND a.attnum = k.n ORDER BY k.o), chr(31))"
        )
    };
    let sql = format!(
        "SELECT con.conname AS name, rn.nspname AS ref_schema, rc.relname AS ref_table, \
         {} AS cols, {} AS ref_cols, con.confupdtype AS on_update, con.confdeltype AS on_delete \
         FROM pg_constraint con JOIN pg_class rc ON rc.oid = con.confrelid \
         JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
         WHERE con.contype = 'f' AND con.conrelid = {} ORDER BY con.conname",
        names("conkey", "conrelid"),
        names("confkey", "confrelid"),
        regclass(schema, table)
    );
    let rs = s.rows(&sql).await?;
    Ok(rs
        .rows
        .iter()
        .map(|r| ForeignKeyInfo {
            name: text(&rs, r, "name"),
            columns: list(&rs, r, "cols"),
            ref_database: text(&rs, r, "ref_schema"),
            ref_table: text(&rs, r, "ref_table"),
            ref_columns: list(&rs, r, "ref_cols"),
            on_update: referential_action(&text(&rs, r, "on_update")).to_owned(),
            on_delete: referential_action(&text(&rs, r, "on_delete")).to_owned(),
        })
        .collect())
}

pub(crate) async fn checks(
    s: &mut PostgresSession,
    schema: &str,
    table: &str,
) -> Result<Vec<CheckInfo>> {
    let sql = format!(
        "SELECT conname AS name, pg_get_constraintdef(oid, true) AS def FROM pg_constraint \
         WHERE contype = 'c' AND conrelid = {} ORDER BY conname",
        regclass(schema, table)
    );
    let rs = s.rows(&sql).await?;
    Ok(rs
        .rows
        .iter()
        .filter_map(|r| {
            // `CHECK ((price > 0))`, maybe followed by `NOT VALID`.
            let def = text(&rs, r, "def");
            let body = def.strip_prefix("CHECK (")?;
            let end = body.rfind(')')?;
            Some(CheckInfo {
                name: text(&rs, r, "name"),
                clause: body[..end].to_owned(),
            })
        })
        .collect())
}

pub(crate) async fn table_options(
    s: &mut PostgresSession,
    schema: &str,
    table: &str,
) -> Result<TableOptions> {
    let rs = s
        .rows(&format!(
            "SELECT coalesce(obj_description({}, 'pg_class'), '') AS comment",
            regclass(schema, table)
        ))
        .await?;
    Ok(TableOptions {
        comment: rs
            .rows
            .first()
            .map(|r| text(&rs, r, "comment"))
            .unwrap_or_default(),
        ..TableOptions::default()
    })
}

/// `CREATE TABLE` with columns and constraints, then its other indexes and comments, as
/// separate statements.
async fn table_ddl(s: &mut PostgresSession, schema: &str, table: &str) -> Result<String> {
    let target = D.qualified(schema, table);
    let cols = s.rows(&columns_query(schema, table)).await?;
    if cols.rows.is_empty() {
        return Err(Error::Other(format!("{table} not found")));
    }
    let mut lines: Vec<String> = cols
        .rows
        .iter()
        .map(|r| {
            let mut line = format!(
                "  {} {}",
                D.quote_ident(&text(&cols, r, "name")),
                text(&cols, r, "type")
            );
            let collation = text(&cols, r, "collation");
            if !collation.is_empty() {
                _ = write!(line, " COLLATE {}", D.quote_ident(&collation));
            }
            let def = text(&cols, r, "def");
            match (
                text(&cols, r, "identity").as_str(),
                text(&cols, r, "generated").as_str(),
            ) {
                ("a", _) => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
                ("d", _) => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                (_, "s") => _ = write!(line, " GENERATED ALWAYS AS ({def}) STORED"),
                _ if !def.is_empty() => _ = write!(line, " DEFAULT {def}"),
                _ => {}
            }
            if text(&cols, r, "notnull") == "true" {
                line.push_str(" NOT NULL");
            }
            line
        })
        .collect();
    let rel = regclass(schema, table);
    let constraints = s
        .rows(&format!(
            "SELECT conname AS name, pg_get_constraintdef(oid, true) AS def FROM pg_constraint \
             WHERE conrelid = {rel} AND contype IN ('p', 'u', 'c', 'f', 'x') \
             ORDER BY contype = 'f', conname"
        ))
        .await?;
    lines.extend(constraints.rows.iter().map(|r| {
        format!(
            "  CONSTRAINT {} {}",
            D.quote_ident(&text(&constraints, r, "name")),
            text(&constraints, r, "def")
        )
    }));
    // Sequences behind serial columns must exist before the table's defaults name them.
    let sequences = s
        .rows(&format!(
            "SELECT format('%I.%I', sn.nspname, sq.relname) AS seq, a.attname AS col \
             FROM pg_depend d JOIN pg_class sq ON sq.oid = d.objid AND sq.relkind = 'S' \
             JOIN pg_namespace sn ON sn.oid = sq.relnamespace \
             JOIN pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid \
             WHERE d.refobjid = {rel} AND d.deptype = 'a' ORDER BY 1"
        ))
        .await?;
    let mut statements: Vec<String> = sequences
        .rows
        .iter()
        .map(|r| {
            format!(
                "CREATE SEQUENCE IF NOT EXISTS {}",
                text(&sequences, r, "seq")
            )
        })
        .collect();
    statements.push(format!("CREATE TABLE {target} (\n{}\n)", lines.join(",\n")));
    statements.extend(sequences.rows.iter().map(|r| {
        format!(
            "ALTER SEQUENCE {} OWNED BY {target}.{}",
            text(&sequences, r, "seq"),
            D.quote_ident(&text(&sequences, r, "col"))
        )
    }));
    let indexes = s
        .rows(&format!(
            "SELECT pg_get_indexdef(i.indexrelid) AS def FROM pg_index i \
             WHERE i.indrelid = {rel} AND NOT EXISTS \
             (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid) \
             ORDER BY 1"
        ))
        .await?;
    statements.extend(indexes.rows.iter().map(|r| text(&indexes, r, "def")));
    let comment = table_options(s, schema, table).await?.comment;
    if !comment.is_empty() {
        statements.push(format!(
            "COMMENT ON TABLE {target} IS {}",
            D.quote_str(&comment)
        ));
    }
    for r in &cols.rows {
        let comment = text(&cols, r, "comment");
        if !comment.is_empty() {
            statements.push(format!(
                "COMMENT ON COLUMN {target}.{} IS {}",
                D.quote_ident(&text(&cols, r, "name")),
                D.quote_str(&comment)
            ));
        }
    }
    Ok(statements.join(";\n"))
}

pub(crate) async fn ddl(
    s: &mut PostgresSession,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<String> {
    let ns = D.quote_str(schema);
    let sql = match kind {
        ObjectKind::Database => return Ok(format!("CREATE SCHEMA {}", D.quote_ident(name))),
        ObjectKind::Table => return table_ddl(s, schema, name).await,
        ObjectKind::View => format!(
            "SELECT CASE c.relkind WHEN 'm' THEN 'CREATE MATERIALIZED VIEW ' \
             ELSE 'CREATE OR REPLACE VIEW ' END || {} || ' AS' || chr(10) || \
             rtrim(pg_get_viewdef(c.oid, true), ';') AS def \
             FROM pg_class c WHERE c.oid = {}",
            D.quote_str(&D.qualified(schema, name)),
            regclass(schema, name)
        ),
        ObjectKind::Procedure | ObjectKind::Function => format!(
            "SELECT rtrim(pg_get_functiondef(p.oid), E' \\n;') AS def FROM pg_proc p \
             JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = {ns} AND p.proname = {} ORDER BY p.oid LIMIT 1",
            D.quote_str(name)
        ),
        ObjectKind::Trigger => format!(
            "SELECT pg_get_triggerdef(t.oid, true) AS def FROM pg_trigger t \
             JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {ns} AND t.tgname = {}",
            D.quote_str(name)
        ),
        ObjectKind::Event => {
            return Err(Error::Unsupported("PostgreSQL has no events".into()));
        }
    };
    let rs = s.rows(&sql).await?;
    rs.rows
        .first()
        .map(|r| text(&rs, r, "def"))
        .filter(|d| !d.is_empty())
        .ok_or_else(|| Error::Other(format!("{name} not found")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_lose_their_casts() {
        assert_eq!(plain_default("'n/a'::text"), "n/a");
        assert_eq!(plain_default("'it''s'::character varying"), "it's");
        assert_eq!(plain_default("now()"), "now()");
        assert_eq!(plain_default("0"), "0");
    }
}
