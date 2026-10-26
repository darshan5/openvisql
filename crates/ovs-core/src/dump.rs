//! SQL dumps of a database, written through the [`Session`] trait so any engine with a
//! driver can be backed up, and restored by running the script (see [`run_script`]).

use std::io::Write;

use crate::driver::Session;
use crate::error::{Error, Result};
use crate::result::QueryOutcome;
use crate::schema::ObjectKind;
use crate::sql::{Dialect, split_statements};

/// Rows fetched per query while dumping table data.
const FETCH_BATCH: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpOptions {
    pub structure: bool,
    pub data: bool,
    /// `DROP ... IF EXISTS` before each `CREATE`.
    pub drop_existing: bool,
    pub views: bool,
    pub routines: bool,
    pub triggers: bool,
    pub events: bool,
    /// Remove `DEFINER=...` so the dump restores under any user.
    pub strip_definer: bool,
    pub rows_per_insert: usize,
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self {
            structure: true,
            data: true,
            drop_existing: true,
            views: true,
            routines: true,
            triggers: true,
            events: true,
            strip_definer: true,
            rows_per_insert: 100,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DumpStats {
    pub tables: usize,
    pub rows: u64,
    pub other_objects: usize,
}

// Takes the error by value so it can be passed straight to `map_err`.
#[allow(clippy::needless_pass_by_value)]
fn io(err: std::io::Error) -> Error {
    Error::Other(format!("writing the dump failed: {err}"))
}

/// Removes a `DEFINER=user@host` clause from a `CREATE` statement.
pub fn strip_definer(ddl: &str) -> String {
    let Some(start) = ddl.find("DEFINER=") else {
        return ddl.to_owned();
    };
    // The clause ends at the first whitespace outside quotes.
    let rest = &ddl[start..];
    let mut quote = None;
    let mut end = rest.len();
    for (i, c) in rest.char_indices() {
        match (quote, c) {
            (None, '`' | '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let after = rest[end..].trim_start();
    format!("{}{after}", &ddl[..start])
}

/// Writes a dump of `db` to `out`. `progress` is told which object is being written.
pub async fn dump_database(
    session: &mut dyn Session,
    db: &str,
    options: &DumpOptions,
    out: &mut (dyn Write + Send),
    progress: &mut (dyn FnMut(&str) + Send),
) -> Result<DumpStats> {
    let d = session.dialect();
    let mut stats = DumpStats::default();
    let clean = |ddl: String| {
        if options.strip_definer {
            strip_definer(&ddl)
        } else {
            ddl
        }
    };
    writeln!(
        out,
        "-- OpenViSQL dump of {}\n-- Server: {}\n\nSET NAMES utf8mb4;\nSET FOREIGN_KEY_CHECKS = 0;\nSET UNIQUE_CHECKS = 0;\n",
        d.quote_ident(db),
        session.server_version()
    )
    .map_err(io)?;

    for table in session.list_objects(db, ObjectKind::Table).await? {
        progress(&table.name);
        let target = d.quote_ident(&table.name);
        if options.structure {
            let ddl = session
                .object_ddl(db, &table.name, ObjectKind::Table)
                .await?;
            writeln!(out, "--\n-- Table {target}\n--").map_err(io)?;
            if options.drop_existing {
                writeln!(out, "DROP TABLE IF EXISTS {target};").map_err(io)?;
            }
            writeln!(out, "{ddl};\n").map_err(io)?;
        }
        if options.data {
            stats.rows += dump_rows(session, d, db, &table.name, options, out).await?;
        }
        stats.tables += 1;
    }

    let mut others = Vec::new();
    if options.views {
        others.push(ObjectKind::View);
    }
    if options.routines {
        others.extend([ObjectKind::Procedure, ObjectKind::Function]);
    }
    if options.triggers {
        others.push(ObjectKind::Trigger);
    }
    if options.events {
        others.push(ObjectKind::Event);
    }
    if options.structure {
        for kind in others {
            for object in session.list_objects(db, kind).await? {
                progress(&object.name);
                let ddl = clean(session.object_ddl(db, &object.name, kind).await?);
                let name = d.quote_ident(&object.name);
                writeln!(out, "--\n-- {} {name}\n--", kind.sql_keyword()).map_err(io)?;
                if options.drop_existing {
                    writeln!(out, "DROP {} IF EXISTS {name};", kind.sql_keyword()).map_err(io)?;
                }
                // Bodies contain `;`, so routines, triggers and events use another delimiter.
                if kind == ObjectKind::View {
                    writeln!(out, "{ddl};\n").map_err(io)?;
                } else {
                    writeln!(out, "DELIMITER $$\n{ddl}$$\nDELIMITER ;\n").map_err(io)?;
                }
                stats.other_objects += 1;
            }
        }
    }
    writeln!(out, "SET UNIQUE_CHECKS = 1;\nSET FOREIGN_KEY_CHECKS = 1;").map_err(io)?;
    out.flush().map_err(io)?;
    Ok(stats)
}

/// Writes `INSERT`s for every row of `table`, fetched in primary-key order in batches.
async fn dump_rows(
    session: &mut dyn Session,
    d: Dialect,
    db: &str,
    table: &str,
    options: &DumpOptions,
    out: &mut (dyn Write + Send),
) -> Result<u64> {
    let columns = session.list_columns(db, table).await?;
    // Generated columns can't be inserted into.
    let insertable: Vec<&str> = columns
        .iter()
        .filter(|c| !c.extra.to_ascii_uppercase().contains("GENERATED"))
        .map(|c| c.name.as_str())
        .collect();
    if insertable.is_empty() {
        return Ok(0);
    }
    let key: Vec<String> = session
        .list_indexes(db, table)
        .await?
        .into_iter()
        .find(crate::schema::IndexInfo::is_primary)
        .map(|i| i.columns.iter().map(|c| d.quote_ident(c)).collect())
        .unwrap_or_default();
    let select_cols = insertable
        .iter()
        .map(|c| d.quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let order = if key.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY {}", key.join(", "))
    };
    let target = d.qualified(db, table);
    let insert_into = format!(
        "INSERT INTO {} ({select_cols}) VALUES",
        d.quote_ident(table)
    );

    let mut offset = 0u64;
    let mut total = 0u64;
    loop {
        let sql =
            format!("SELECT {select_cols} FROM {target}{order} LIMIT {offset}, {FETCH_BATCH}");
        let rows = match session.execute(&sql, None).await?.into_iter().next() {
            Some(QueryOutcome::Rows(rs)) => rs.rows,
            _ => Vec::new(),
        };
        let fetched = rows.len() as u64;
        for chunk in rows.chunks(options.rows_per_insert.max(1)) {
            let values = chunk
                .iter()
                .map(|row| {
                    let vals: Vec<String> = row.iter().map(|v| d.literal(v)).collect();
                    format!("({})", vals.join(","))
                })
                .collect::<Vec<_>>()
                .join(",\n");
            writeln!(out, "{insert_into}\n{values};").map_err(io)?;
        }
        total += fetched;
        if fetched < FETCH_BATCH {
            break;
        }
        offset += FETCH_BATCH;
    }
    if total > 0 {
        writeln!(out).map_err(io)?;
    }
    Ok(total)
}

/// Outcome of running a script.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptStats {
    pub executed: usize,
    /// Failed statements: (statement number, error message).
    pub errors: Vec<(usize, String)>,
}

/// Runs every statement of `script` (honouring `DELIMITER`). With `stop_on_error`, the first
/// failure ends the run. `progress` gets (statements done, total).
pub async fn run_script(
    session: &mut dyn Session,
    script: &str,
    stop_on_error: bool,
    progress: &mut (dyn FnMut(usize, usize) + Send),
) -> ScriptStats {
    let statements = split_statements(script);
    let total = statements.len();
    let mut stats = ScriptStats::default();
    for (i, stmt) in statements.iter().enumerate() {
        match session.execute(&stmt.sql, Some(0)).await {
            Ok(_) => stats.executed += 1,
            Err(err) => {
                stats.errors.push((i + 1, err.to_string()));
                if stop_on_error {
                    break;
                }
            }
        }
        progress(i + 1, total);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_definer_clauses() {
        assert_eq!(
            strip_definer("CREATE DEFINER=`root`@`%` PROCEDURE `p`() BEGIN END"),
            "CREATE PROCEDURE `p`() BEGIN END"
        );
        assert_eq!(
            strip_definer(
                "CREATE ALGORITHM=UNDEFINED DEFINER=`a b`@`localhost` SQL SECURITY DEFINER VIEW `v` AS select 1"
            ),
            "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS select 1"
        );
        assert_eq!(
            strip_definer("CREATE TABLE t (a int)"),
            "CREATE TABLE t (a int)"
        );
    }
}
