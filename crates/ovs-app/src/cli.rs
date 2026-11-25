//! `openvisql query`: run SQL against a saved connection from the command line and print the
//! results, without opening a window. Meant for scripts and for remote machines driven
//! through a Run box, where a result file is easier to read than a screen.
//!
//! On Windows the app is a GUI program, so nothing it prints reaches a console: redirect
//! (`> out.txt 2>&1`) or use `--out`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

use ovs_core::export::{ExportFormat, ExportOptions, export_to_string};
use ovs_core::sql::{self, StatementKind};
use ovs_core::{ConnectionConfig, QueryOutcome, plain_text};
use ovs_store::Store;

use crate::runtime;

const USAGE: &str = "\
Usage: openvisql query --conn <name|id> (--file <path> | --string <sql> |
                       --snippet <name> [--param name=value ...]) [options]
       openvisql query --list-snippets
       openvisql query --save-snippet <name> (--file <path> | --string <sql>)
       openvisql query --delete-snippet <name>

Runs SQL against a saved connection and prints the results. No window opens.

  --conn <name|id>     Saved connection, by name (ignoring case) or id
  --file <path>        Read the SQL from a file (one or more statements)
  --string <sql>       The SQL itself. Backslash escapes are understood:
                       \\n newline, \\t tab, \\\" quote, \\' apostrophe, \\\\ backslash
  --snippet <name>     Run a saved query (F2 in the app), by name, ignoring case
  --param name=value   Fill its {{name}} placeholder; repeat for each one. The value
                       goes in exactly as typed (the query decides about quotes)
  --list-snippets      List the saved queries and the parameters each one needs
  --save-snippet <name>   Save --file or --string as a saved query (replaces one
                       with the same name). Use {{name}} for values to fill in
  --delete-snippet <name> Delete a saved query
  --database <name>    Use this database first (like USE)
  --format <fmt>       text (default), records, csv, tsv, json, markdown
  --out <path>         Write the output to a file instead of standard output
  --limit <n>          Add LIMIT n to SELECTs that have none (default: no limit)
  --read-only          Refuse writes and open the session read-only
  --yes                Allow UPDATE/DELETE without WHERE (when the connection asks
                       before them) and writes on connections that preview them
  --help               Show this help

The password comes from where the connection saved it, or from the
OPENVISQL_PASSWORD environment variable. SSH-tunnelled connections aren't
supported here yet.

Exit codes: 0 success, 1 a statement failed, 2 bad arguments or connection,
3 refused (read-only connection or a safety check).

On Windows, nothing reaches the console: use --out, or redirect with
> out.txt 2>&1.

Examples:
  openvisql query --conn checkposting --string \"SHOW DATABASES;\"
  openvisql query --conn checkposting --file C:\\temp\\q.sql --format records --out C:\\temp\\r.txt
  openvisql query --conn checkposting --snippet \"Check lines\" --param check=46411 --out C:\\temp\\r.txt
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Records,
    Csv,
    Tsv,
    Json,
    Markdown,
}

#[derive(Debug, PartialEq, Eq)]
enum Source {
    File(PathBuf),
    Text(String),
    /// A saved query by name, with its `--param` values.
    Snippet {
        name: String,
        params: Vec<(String, String)>,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    conn: String,
    source: Source,
    database: Option<String>,
    format: Format,
    out: Option<PathBuf>,
    limit: Option<usize>,
    read_only: bool,
    yes: bool,
}

/// Why the run stopped early, with its exit code.
struct Failure {
    code: i32,
    message: String,
}

fn fail(code: i32, message: impl Into<String>) -> Failure {
    Failure {
        code,
        message: message.into(),
    }
}

/// Runs `openvisql query` with the arguments after `query`, and returns the exit code.
pub fn main(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return 0;
    }
    if args.iter().any(|a| a == "--list-snippets") {
        return list_snippets();
    }
    if let Some(code) = manage_snippets(args) {
        return code;
    }
    let args = match parse(args) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("openvisql query: {message}\n\n{USAGE}");
            return 2;
        }
    };
    let mut output = String::new();
    let code = match run(&args, &mut output) {
        Ok(()) => 0,
        Err(failure) => {
            let _ = writeln!(output, "ERROR: {}", failure.message);
            eprintln!("openvisql query: {}", failure.message);
            failure.code
        }
    };
    let written = match &args.out {
        Some(path) => std::fs::write(path, &output).map_err(|e| (path.display().to_string(), e)),
        None => std::io::stdout()
            .write_all(output.as_bytes())
            .map_err(|e| ("standard output".to_owned(), e)),
    };
    if let Err((target, err)) = written {
        eprintln!("openvisql query: could not write {target}: {err}");
        return 2;
    }
    code
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut conn = None;
    let mut source = None;
    let mut database = None;
    let mut format = Format::Text;
    let mut out = None;
    let mut limit = None;
    let mut read_only = false;
    let mut yes = false;
    let mut params: Vec<(String, String)> = Vec::new();
    let mut it = args.iter();
    let two_sources = "give only one of --file, --string and --snippet";
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--conn" => conn = Some(value("--conn")?),
            "--file" => {
                if source.is_some() {
                    return Err(two_sources.into());
                }
                source = Some(Source::File(PathBuf::from(value("--file")?)));
            }
            "--string" => {
                if source.is_some() {
                    return Err(two_sources.into());
                }
                source = Some(Source::Text(unescape(&value("--string")?)));
            }
            "--snippet" => {
                if source.is_some() {
                    return Err(two_sources.into());
                }
                source = Some(Source::Snippet {
                    name: value("--snippet")?,
                    params: Vec::new(),
                });
            }
            "--param" => {
                let pair = value("--param")?;
                let (name, val) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("--param needs name=value, not {pair}"))?;
                params.push((name.trim().to_owned(), val.to_owned()));
            }
            "--database" => database = Some(value("--database")?),
            "--format" => {
                format = match value("--format")?.to_ascii_lowercase().as_str() {
                    "text" | "table" => Format::Text,
                    "records" | "vertical" => Format::Records,
                    "csv" => Format::Csv,
                    "tsv" => Format::Tsv,
                    "json" => Format::Json,
                    "markdown" | "md" => Format::Markdown,
                    other => return Err(format!("unknown format \u{201c}{other}\u{201d}")),
                };
            }
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            "--limit" => {
                let text = value("--limit")?;
                limit = Some(
                    text.parse::<usize>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("--limit must be a positive number, not {text}"))?,
                );
            }
            "--read-only" => read_only = true,
            "--yes" => yes = true,
            other => return Err(format!("unknown argument \u{201c}{other}\u{201d}")),
        }
    }
    let source = match source {
        Some(Source::Snippet { name, .. }) => Source::Snippet { name, params },
        Some(_) if !params.is_empty() => return Err("--param only goes with --snippet".into()),
        Some(other) => other,
        None => return Err("give the SQL with --file, --string or --snippet".into()),
    };
    Ok(Args {
        conn: conn.ok_or("--conn is required")?,
        source,
        database,
        format,
        out,
        limit,
        read_only,
        yes,
    })
}

/// Backslash escapes in `--string`: `\n`, `\t`, `\r`, `\"`, `\'` and `\\`. Any other
/// backslash is kept as typed, so `LIKE 'a\_b'` arrives unchanged.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(&q @ ('"' | '\'' | '\\')) => out.push(q),
            _ => {
                out.push('\\');
                continue;
            }
        }
        chars.next();
    }
    out
}

/// Prints the saved queries with the parameters each needs, for `--list-snippets`.
fn list_snippets() -> i32 {
    match Store::default().load_snippets() {
        Ok(snippets) if snippets.is_empty() => {
            println!("No saved queries. Save one in the app with Ctrl+Shift+K (⌘⇧K).");
            0
        }
        Ok(snippets) => {
            for snippet in snippets {
                let params = sql::snippets::params(&snippet.sql);
                if params.is_empty() {
                    println!("{}", snippet.name);
                } else {
                    let flags: Vec<String> =
                        params.iter().map(|p| format!("--param {p}=…")).collect();
                    println!("{}  ({})", snippet.name, flags.join(" "));
                }
            }
            0
        }
        Err(err) => {
            eprintln!("openvisql query: could not read saved queries: {err}");
            2
        }
    }
}

/// What `--save-snippet` / `--delete-snippet` asked for.
#[derive(Debug, PartialEq, Eq)]
enum SnippetChange {
    Save { name: String, sql: String },
    Delete(String),
}

/// Reads `--save-snippet <name> (--file | --string)` or `--delete-snippet <name>`; `None`
/// when neither is given.
fn parse_snippet_change(args: &[String]) -> Option<Result<SnippetChange, String>> {
    let value_after = |flag: &str| {
        args.iter().position(|a| a == flag).map(|i| {
            args.get(i + 1)
                .cloned()
                .ok_or(format!("{flag} needs a name"))
        })
    };
    if let Some(name) = value_after("--delete-snippet") {
        return Some(name.map(SnippetChange::Delete));
    }
    let name = value_after("--save-snippet")?;
    Some(name.and_then(|name| {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err("--save-snippet needs a name".into());
        }
        let sql = match (value_after("--string"), value_after("--file")) {
            (Some(_), Some(_)) => return Err("give either --file or --string, not both".into()),
            (Some(text), None) => unescape(&text?),
            (None, Some(path)) => {
                let path = path?;
                std::fs::read_to_string(&path).map_err(|e| format!("could not read {path}: {e}"))?
            }
            (None, None) => {
                return Err("--save-snippet needs the SQL with --file or --string".into());
            }
        };
        let sql = sql.trim().to_owned();
        if sql.is_empty() {
            return Err("there's no SQL to save".into());
        }
        Ok(SnippetChange::Save { name, sql })
    }))
}

/// Handles `--save-snippet` and `--delete-snippet`, returning the exit code, or `None` when
/// the arguments are for running a query instead.
fn manage_snippets(args: &[String]) -> Option<i32> {
    let change = match parse_snippet_change(args)? {
        Ok(change) => change,
        Err(message) => {
            eprintln!("openvisql query: {message}");
            return Some(2);
        }
    };
    let store = Store::default();
    let existing = match store.load_snippets() {
        Ok(snippets) => snippets,
        Err(err) => {
            eprintln!("openvisql query: could not read saved queries: {err}");
            return Some(2);
        }
    };
    let exists = |name: &str| existing.iter().any(|s| s.name.eq_ignore_ascii_case(name));
    let result = match &change {
        SnippetChange::Save { name, sql } => {
            let replaced = exists(name);
            store
                .upsert_snippet(ovs_store::Snippet {
                    name: name.clone(),
                    sql: sql.clone(),
                })
                .map(|()| {
                    let params = sql::snippets::params(sql);
                    format!(
                        "{} \u{201c}{name}\u{201d}{}",
                        if replaced { "Replaced" } else { "Saved" },
                        if params.is_empty() {
                            String::new()
                        } else {
                            format!(" (asks for: {})", params.join(", "))
                        }
                    )
                })
        }
        SnippetChange::Delete(name) => {
            if !exists(name) {
                eprintln!("openvisql query: no saved query called \u{201c}{name}\u{201d}");
                return Some(2);
            }
            store
                .delete_snippet(name)
                .map(|()| format!("Deleted \u{201c}{name}\u{201d}"))
        }
    };
    match result {
        Ok(message) => {
            println!("{message}");
            Some(0)
        }
        Err(err) => {
            eprintln!("openvisql query: could not save: {err}");
            Some(2)
        }
    }
}

/// The SQL of the saved query called `name` (ignoring case) with `params` filled in. Every
/// placeholder needs a value, and every value a placeholder, so a typo can't run a query with
/// `{{check}}` left in it.
fn resolve_snippet(
    snippets: &[ovs_store::Snippet],
    name: &str,
    params: &[(String, String)],
) -> Result<String, String> {
    let snippet = snippets
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let names: Vec<&str> = snippets.iter().map(|s| s.name.as_str()).collect();
            format!(
                "no saved query called \u{201c}{name}\u{201d} (saved: {})",
                if names.is_empty() {
                    "none".to_owned()
                } else {
                    names.join(", ")
                }
            )
        })?;
    let wanted = sql::snippets::params(&snippet.sql);
    let missing: Vec<&String> = wanted
        .iter()
        .filter(|w| !params.iter().any(|(n, _)| n == *w))
        .collect();
    if !missing.is_empty() {
        let flags: Vec<String> = missing.iter().map(|m| format!("--param {m}=…")).collect();
        return Err(format!(
            "\u{201c}{}\u{201d} needs {}",
            snippet.name,
            flags.join(" ")
        ));
    }
    if let Some((unknown, _)) = params.iter().find(|(n, _)| !wanted.contains(n)) {
        return Err(format!(
            "\u{201c}{}\u{201d} has no {{{{{unknown}}}}} parameter (it takes: {})",
            snippet.name,
            if wanted.is_empty() {
                "none".to_owned()
            } else {
                wanted.join(", ")
            }
        ));
    }
    let values: std::collections::HashMap<String, String> = params.iter().cloned().collect();
    Ok(sql::snippets::fill(&snippet.sql, &values))
}

/// The saved connection called `name` (ignoring case), or with that id.
fn find_connection(all: Vec<ConnectionConfig>, name: &str) -> Option<ConnectionConfig> {
    all.into_iter()
        .find(|c| c.name.eq_ignore_ascii_case(name) || c.id.to_string() == name)
}

fn run(args: &Args, out: &mut String) -> Result<(), Failure> {
    let store = Store::default();
    let connections = store
        .load_connections()
        .map_err(|e| fail(2, format!("could not read saved connections: {e}")))?;
    let names: Vec<String> = connections.iter().map(|c| c.name.clone()).collect();
    let mut config = find_connection(connections, &args.conn).ok_or_else(|| {
        fail(
            2,
            format!(
                "no saved connection called \u{201c}{}\u{201d} (saved: {})",
                args.conn,
                names.join(", ")
            ),
        )
    })?;
    if config.ssh.enabled {
        return Err(fail(
            2,
            "SSH-tunnelled connections can't be used from the command line yet",
        ));
    }
    config.read_only |= args.read_only;

    let sql_text = match &args.source {
        Source::Text(text) => text.clone(),
        Source::File(path) => std::fs::read_to_string(path)
            .map_err(|e| fail(2, format!("could not read {}: {e}", path.display())))?,
        Source::Snippet { name, params } => {
            let snippets = store
                .load_snippets()
                .map_err(|e| fail(2, format!("could not read saved queries: {e}")))?;
            resolve_snippet(&snippets, name, params).map_err(|e| fail(2, e))?
        }
    };
    let dialect = config.engine.dialect();
    let statements = sql::split_statements_for(&sql_text, dialect);
    if statements.is_empty() {
        return Err(fail(2, "there's no SQL to run"));
    }
    check_allowed(&config, &statements, args.yes)?;

    let password = match std::env::var("OPENVISQL_PASSWORD") {
        Ok(p) => Some(p),
        Err(_) if config.save_password && config.engine.is_server() => store
            .password(config.id, config.password_store)
            .map_err(|e| fail(2, format!("could not read the saved password: {e}")))?,
        Err(_) => None,
    };

    let database = args.database.clone();
    let limit = args.limit;
    let format = args.format;
    let reports = runtime::runtime().block_on(async move {
        let session = runtime::connect(config, password)
            .await
            .map_err(|e| fail(2, format!("could not connect: {e}")))?;
        let mut session = session.lock().await;
        if let Some(db) = &database {
            session
                .use_database(db)
                .await
                .map_err(|e| fail(2, format!("USE {db}: {e}")))?;
        }
        let mut reports = Vec::new();
        for stmt in statements {
            let to_run = limit
                .and_then(|n| sql::apply_row_limit(&stmt.sql, n))
                .unwrap_or_else(|| stmt.sql.clone());
            let result = session.execute(&to_run, None).await;
            let failed = result.is_err();
            reports.push((stmt.sql, result));
            if failed {
                break;
            }
        }
        Ok::<_, Failure>(reports)
    })?;

    let many = reports.len() > 1;
    for (sql_text, result) in reports {
        if many && matches!(format, Format::Text | Format::Records) {
            let _ = writeln!(
                out,
                "-- {}",
                sql_text.split_whitespace().collect::<Vec<_>>().join(" ")
            );
        }
        let outcomes = result.map_err(|e| fail(1, e.to_string()))?;
        for outcome in outcomes {
            write_outcome(&outcome, format, out);
        }
    }
    Ok(())
}

/// Refuses statements the connection's settings don't allow without asking.
fn check_allowed(
    config: &ConnectionConfig,
    statements: &[sql::Statement],
    yes: bool,
) -> Result<(), Failure> {
    let kinds: Vec<StatementKind> = statements.iter().map(|s| sql::classify(&s.sql)).collect();
    if config.read_only && kinds.iter().any(StatementKind::is_write) {
        return Err(fail(
            3,
            "the connection is read-only and the SQL would change data or schema",
        ));
    }
    if config.safe_updates && !yes && statements.iter().any(|s| sql::missing_where(&s.sql)) {
        return Err(fail(
            3,
            "an UPDATE or DELETE has no WHERE clause; add --yes to run it anyway",
        ));
    }
    if config.confirm_writes
        && !yes
        && kinds
            .iter()
            .any(|k| matches!(k, StatementKind::Update | StatementKind::Delete))
    {
        return Err(fail(
            3,
            "the connection previews UPDATE/DELETE before running them; add --yes to run \
             without the preview",
        ));
    }
    Ok(())
}

fn write_outcome(outcome: &QueryOutcome, format: Format, out: &mut String) {
    match outcome {
        QueryOutcome::Rows(rs) => {
            let text = match format {
                Format::Text => plain_text::text_table(rs, None, 0),
                Format::Records => plain_text::records(rs, None, 0),
                Format::Csv | Format::Tsv | Format::Json | Format::Markdown => {
                    let options = ExportOptions {
                        format: match format {
                            Format::Json => ExportFormat::Json,
                            Format::Markdown => ExportFormat::Markdown,
                            _ => ExportFormat::Csv,
                        },
                        delimiter: if format == Format::Tsv { '\t' } else { ',' },
                        ..ExportOptions::default()
                    };
                    export_to_string(rs, &options)
                }
            };
            out.push_str(&text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
            if matches!(format, Format::Text | Format::Records) {
                let n = rs.rows.len();
                let _ = writeln!(out, "{n} row{} returned\n", if n == 1 { "" } else { "s" });
            }
        }
        QueryOutcome::Affected { rows, .. } => {
            let _ = writeln!(
                out,
                "{rows} row{} affected",
                if *rows == 1 { "" } else { "s" }
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_a_string_query() {
        let parsed = parse(&args(&[
            "--conn",
            "checkposting",
            "--string",
            "SHOW DATABASES;",
            "--format",
            "records",
            "--out",
            "r.txt",
        ]))
        .unwrap();
        assert_eq!(parsed.conn, "checkposting");
        assert_eq!(parsed.source, Source::Text("SHOW DATABASES;".into()));
        assert_eq!(parsed.format, Format::Records);
        assert_eq!(parsed.out, Some(PathBuf::from("r.txt")));
        assert!(!parsed.read_only && !parsed.yes);
    }

    #[test]
    fn rejects_bad_arguments() {
        assert!(parse(&args(&["--string", "select 1"])).is_err());
        assert!(parse(&args(&["--conn", "c"])).is_err());
        assert!(parse(&args(&["--conn", "c", "--file", "a", "--string", "b"])).is_err());
        assert!(parse(&args(&["--conn", "c", "--string", "x", "--format", "xml"])).is_err());
        assert!(parse(&args(&["--conn", "c", "--string", "x", "--limit", "0"])).is_err());
        assert!(parse(&args(&["--conn", "c", "--string"])).is_err());
        assert!(parse(&args(&["--conn", "c", "--string", "x", "--bogus"])).is_err());
    }

    #[test]
    fn parses_a_snippet_with_params() {
        let parsed = parse(&args(&[
            "--conn",
            "c",
            "--snippet",
            "Check lines",
            "--param",
            "check=46411",
            "--param",
            "note=a=b",
        ]))
        .unwrap();
        assert_eq!(
            parsed.source,
            Source::Snippet {
                name: "Check lines".into(),
                params: vec![
                    ("check".into(), "46411".into()),
                    ("note".into(), "a=b".into())
                ],
            }
        );
        assert!(parse(&args(&["--conn", "c", "--string", "x", "--param", "a=1"])).is_err());
        assert!(
            parse(&args(&[
                "--conn",
                "c",
                "--snippet",
                "s",
                "--param",
                "novalue"
            ]))
            .is_err()
        );
        assert!(parse(&args(&["--conn", "c", "--snippet", "s", "--string", "x"])).is_err());
    }

    #[test]
    fn parses_snippet_changes() {
        assert_eq!(
            parse_snippet_change(&args(&["--conn", "c", "--string", "x"])),
            None
        );
        assert_eq!(
            parse_snippet_change(&args(&[
                "--save-snippet",
                " Lines ",
                "--string",
                "SELECT\\n1"
            ])),
            Some(Ok(SnippetChange::Save {
                name: "Lines".into(),
                sql: "SELECT\n1".into()
            }))
        );
        assert_eq!(
            parse_snippet_change(&args(&["--delete-snippet", "Lines"])),
            Some(Ok(SnippetChange::Delete("Lines".into())))
        );
        for bad in [
            &["--save-snippet", "x"][..],
            &["--save-snippet"],
            &["--delete-snippet"],
            &["--save-snippet", "x", "--string", "  "],
            &["--save-snippet", "x", "--string", "a", "--file", "b"],
        ] {
            assert!(
                matches!(parse_snippet_change(&args(bad)), Some(Err(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn resolves_snippets_strictly() {
        let saved = [ovs_store::Snippet {
            name: "Check lines".into(),
            sql: "SELECT * FROM checks WHERE CheckNumber = {{check}}".into(),
        }];
        let p = |n: &str, v: &str| (n.to_owned(), v.to_owned());
        assert_eq!(
            resolve_snippet(&saved, "check LINES", &[p("check", "46411")]).unwrap(),
            "SELECT * FROM checks WHERE CheckNumber = 46411"
        );
        let missing = resolve_snippet(&saved, "Check lines", &[]).unwrap_err();
        assert!(missing.contains("--param check="), "{missing}");
        let unknown =
            resolve_snippet(&saved, "Check lines", &[p("check", "1"), p("chek", "2")]).unwrap_err();
        assert!(unknown.contains("{{chek}}"), "{unknown}");
        let none = resolve_snippet(&saved, "nope", &[]).unwrap_err();
        assert!(none.contains("saved: Check lines"), "{none}");
    }

    #[test]
    fn unescapes_only_known_sequences() {
        assert_eq!(
            unescape(r#"SELECT \"a\"\nFROM t WHERE x = \'y\' AND p LIKE 'a\_b' AND q = '\\'"#),
            "SELECT \"a\"\nFROM t WHERE x = 'y' AND p LIKE 'a\\_b' AND q = '\\'"
        );
        assert_eq!(unescape("tab\\there"), "tab\there");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn finds_connections_by_name_or_id() {
        let c = ConnectionConfig {
            name: "CheckPosting".into(),
            ..ConnectionConfig::default()
        };
        let id = c.id.to_string();
        assert!(find_connection(vec![c.clone()], "checkposting").is_some());
        assert!(find_connection(vec![c.clone()], &id).is_some());
        assert!(find_connection(vec![c], "other").is_none());
    }

    #[test]
    fn safety_checks_refuse_without_yes() {
        let stmts = sql::split_statements("UPDATE t SET a = 1");
        let mut config = ConnectionConfig::default();
        assert_eq!(
            check_allowed(&config, &stmts, false).err().map(|f| f.code),
            Some(3)
        );
        assert!(check_allowed(&config, &stmts, true).is_ok());
        config.safe_updates = false;
        assert!(check_allowed(&config, &stmts, false).is_ok());
        config.read_only = true;
        assert_eq!(
            check_allowed(&config, &stmts, true).err().map(|f| f.code),
            Some(3)
        );
        let read = sql::split_statements("SELECT 1; SHOW TABLES");
        assert!(check_allowed(&config, &read, false).is_ok());
        config.read_only = false;
        config.confirm_writes = true;
        let guarded = sql::split_statements("DELETE FROM t WHERE id = 1");
        assert!(check_allowed(&config, &guarded, false).is_err());
        assert!(check_allowed(&config, &guarded, true).is_ok());
    }

    #[test]
    fn writes_rows_and_counts() {
        let rs = ovs_core::ResultSet {
            columns: vec![ovs_core::Column {
                name: "n".into(),
                ..ovs_core::Column::default()
            }],
            rows: vec![vec![ovs_core::Value::Int(1)]],
            truncated: false,
        };
        let mut out = String::new();
        write_outcome(&QueryOutcome::Rows(rs.clone()), Format::Csv, &mut out);
        assert_eq!(out, "n\n1\n");
        out.clear();
        write_outcome(&QueryOutcome::Rows(rs), Format::Records, &mut out);
        assert!(out.ends_with("n: 1\n1 row returned\n\n"));
        out.clear();
        write_outcome(
            &QueryOutcome::Affected {
                rows: 6,
                last_insert_id: None,
                warnings: 0,
                info: String::new(),
            },
            Format::Text,
            &mut out,
        );
        assert_eq!(out, "6 rows affected\n");
    }
}
