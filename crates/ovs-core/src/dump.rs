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
    /// Only these tables (and their triggers); `None` for all of them.
    pub tables: Option<Vec<String>>,
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
            tables: None,
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

/// Where dumped statements go: a script file, or straight into another session (copying).
enum Output<'a> {
    Script(&'a mut (dyn Write + Send)),
    Session(&'a mut dyn Session),
}

impl Output<'_> {
    /// One statement; `compound` ones (routine bodies) need another delimiter in a script.
    async fn statement(&mut self, sql: &str, compound: bool) -> Result<()> {
        match self {
            Self::Script(out) if compound => {
                writeln!(out, "DELIMITER $$\n{sql}$$\nDELIMITER ;\n").map_err(io)
            }
            Self::Script(out) => writeln!(out, "{sql};").map_err(io),
            Self::Session(session) => session.execute(sql, Some(0)).await.map(|_| ()),
        }
    }

    /// Text that only appears in scripts (comments, blank lines).
    fn note(&mut self, text: &str) -> Result<()> {
        match self {
            Self::Script(out) => writeln!(out, "{text}").map_err(io),
            Self::Session(_) => Ok(()),
        }
    }
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
    writeln!(
        out,
        "-- OpenViSQL dump of {}\n-- Server: {}\n",
        d.quote_ident(db),
        session.server_version()
    )
    .map_err(io)?;
    let mut output = Output::Script(out);
    let stats = transfer(session, db, &|ddl, _| ddl, options, &mut output, progress).await?;
    out.flush().map_err(io)?;
    Ok(stats)
}

/// Copies `db` (or the tables in `options.tables`) from `source` into `target_db` on
/// `target`, creating the database if needed. Views are pointed at `target_db`; routines,
/// triggers and events are copied as their DDL reads. Collations the target server doesn't
/// have (copying between MySQL and MariaDB) become its default for the character set.
pub async fn copy_database(
    source: &mut dyn Session,
    db: &str,
    target: &mut dyn Session,
    target_db: &str,
    options: &DumpOptions,
    progress: &mut (dyn FnMut(&str) + Send),
) -> Result<DumpStats> {
    let d = target.dialect();
    target
        .execute(
            &format!("CREATE DATABASE IF NOT EXISTS {}", d.quote_ident(target_db)),
            Some(0),
        )
        .await?;
    target.use_database(target_db).await?;
    let collations = Collations::of(target).await?;
    let (from, to) = (
        format!("{}.", d.quote_ident(db)),
        format!("{}.", d.quote_ident(target_db)),
    );
    let adapt = move |ddl: String, kind: ObjectKind| {
        let ddl = if kind == ObjectKind::View && from != to {
            ddl.replace(&from, &to)
        } else {
            ddl
        };
        collations.adapt(&ddl)
    };
    let mut output = Output::Session(target);
    let result = transfer(source, db, &adapt, options, &mut output, progress).await;
    // Restore the checks even when the copy failed part-way.
    let Output::Session(target) = output else {
        unreachable!("output is a session")
    };
    let restored = target
        .execute("SET UNIQUE_CHECKS = 1, FOREIGN_KEY_CHECKS = 1", Some(0))
        .await;
    let stats = result?;
    restored?;
    Ok(stats)
}

/// The collations a server knows, and its default one per character set.
#[derive(Debug, Clone, Default)]
pub struct Collations {
    known: std::collections::HashSet<String>,
    defaults: std::collections::HashMap<String, String>,
}

impl Collations {
    async fn of(session: &mut dyn Session) -> Result<Self> {
        let mut out = Self::default();
        let Some(QueryOutcome::Rows(rs)) = session
            .execute("SHOW COLLATION", None)
            .await?
            .into_iter()
            .next()
        else {
            return Ok(out);
        };
        let col = |name: &str| {
            rs.columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
        };
        let (Some(name), Some(charset), Some(default)) =
            (col("Collation"), col("Charset"), col("Default"))
        else {
            return Ok(out);
        };
        for row in &rs.rows {
            let collation = row[name].to_string().to_ascii_lowercase();
            if row[default].to_string().eq_ignore_ascii_case("yes") {
                out.defaults.insert(
                    row[charset].to_string().to_ascii_lowercase(),
                    collation.clone(),
                );
            }
            out.known.insert(collation);
        }
        Ok(out)
    }

    /// Replaces every `COLLATE x` / `COLLATE=x` naming a collation this server lacks.
    pub fn adapt(&self, ddl: &str) -> String {
        if self.known.is_empty() {
            return ddl.to_owned();
        }
        let mut out = String::with_capacity(ddl.len());
        let mut rest = ddl;
        while let Some(at) = rest.find("COLLATE") {
            let (before, after) = rest.split_at(at + "COLLATE".len());
            out.push_str(before);
            let gap = after.len() - after.trim_start_matches([' ', '=']).len();
            let (sep, tail) = after.split_at(gap);
            out.push_str(sep);
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            let (name, tail) = tail.split_at(end);
            let lower = name.to_ascii_lowercase();
            if gap == 0 || name.is_empty() || self.known.contains(&lower) {
                out.push_str(name);
            } else {
                let charset = lower.split('_').next().unwrap_or_default();
                let replacement = self
                    .defaults
                    .get(charset)
                    .or_else(|| self.defaults.get("utf8mb4"))
                    .map_or(name, String::as_str);
                out.push_str(replacement);
            }
            rest = tail;
        }
        out.push_str(rest);
        out
    }
}

/// Writes `db`'s objects to `out`, passing each `CREATE` through `adapt` first.
async fn transfer(
    session: &mut dyn Session,
    db: &str,
    adapt: &(dyn Fn(String, ObjectKind) -> String + Send + Sync),
    options: &DumpOptions,
    out: &mut Output<'_>,
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
    for sql in [
        "SET NAMES utf8mb4",
        "SET FOREIGN_KEY_CHECKS = 0",
        "SET UNIQUE_CHECKS = 0",
    ] {
        out.statement(sql, false).await?;
    }
    out.note("")?;

    let tables = session.list_objects(db, ObjectKind::Table).await?;
    let wanted = |name: &str| {
        options
            .tables
            .as_ref()
            .is_none_or(|only| only.iter().any(|t| t == name))
    };
    for table in tables.iter().filter(|t| wanted(&t.name)) {
        progress(&table.name);
        let target = d.quote_ident(&table.name);
        if options.structure {
            let ddl = adapt(
                session
                    .object_ddl(db, &table.name, ObjectKind::Table)
                    .await?,
                ObjectKind::Table,
            );
            out.note(&format!("--\n-- Table {target}\n--"))?;
            if options.drop_existing {
                out.statement(&format!("DROP TABLE IF EXISTS {target}"), false)
                    .await?;
            }
            out.statement(&ddl, false).await?;
            out.note("")?;
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
                // A trigger belongs to its table, which may not have been included.
                if kind == ObjectKind::Trigger && !wanted(&object.detail) {
                    continue;
                }
                progress(&object.name);
                let ddl = adapt(
                    clean(session.object_ddl(db, &object.name, kind).await?),
                    kind,
                );
                let name = d.quote_ident(&object.name);
                out.note(&format!("--\n-- {} {name}\n--", kind.sql_keyword()))?;
                if options.drop_existing {
                    out.statement(
                        &format!("DROP {} IF EXISTS {name}", kind.sql_keyword()),
                        false,
                    )
                    .await?;
                }
                // Bodies contain `;`, so routines, triggers and events use another delimiter.
                out.statement(&ddl, kind != ObjectKind::View).await?;
                out.note("")?;
                stats.other_objects += 1;
            }
        }
    }
    out.statement("SET UNIQUE_CHECKS = 1", false).await?;
    out.statement("SET FOREIGN_KEY_CHECKS = 1", false).await?;
    Ok(stats)
}

/// Writes `INSERT`s for every row of `table`, fetched in primary-key order in batches.
async fn dump_rows(
    session: &mut dyn Session,
    d: Dialect,
    db: &str,
    table: &str,
    options: &DumpOptions,
    out: &mut Output<'_>,
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
            out.statement(&format!("{insert_into}\n{values}"), false)
                .await?;
        }
        total += fetched;
        if fetched < FETCH_BATCH {
            break;
        }
        offset += FETCH_BATCH;
    }
    if total > 0 {
        out.note("")?;
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
    fn adapts_unknown_collations() {
        let collations = Collations {
            known: ["utf8mb4_0900_ai_ci", "utf8mb4_bin", "latin1_swedish_ci"]
                .map(String::from)
                .into(),
            defaults: [
                ("utf8mb4".to_owned(), "utf8mb4_0900_ai_ci".to_owned()),
                ("latin1".to_owned(), "latin1_swedish_ci".to_owned()),
            ]
            .into(),
        };
        assert_eq!(
            collations.adapt(
                "CREATE TABLE t (a varchar(5) COLLATE utf8mb4_bin, b text COLLATE \
                 latin1_german1_ci) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 \
                 COLLATE=utf8mb4_uca1400_ai_ci"
            ),
            "CREATE TABLE t (a varchar(5) COLLATE utf8mb4_bin, b text COLLATE \
             latin1_swedish_ci) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 \
             COLLATE=utf8mb4_0900_ai_ci"
        );
        assert_eq!(Collations::default().adapt("x COLLATE=y"), "x COLLATE=y");
    }

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
