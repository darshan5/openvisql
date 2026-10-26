//! Tests against a real server. Skipped unless `OVS_TEST_PG` is `host:port:user:password`:
//!
//! ```sh
//! docker run -d --name ovs-postgres -e POSTGRES_PASSWORD=secret -p 35432:5432 postgres:17
//! OVS_TEST_PG=127.0.0.1:35432:postgres:secret cargo test -p ovs-postgres --test live
//! ```
//!
//! Each test works in its own schema so tests can run in parallel.

use std::time::Duration;

use ovs_core::editing::{EditTarget, PendingEdits};
use ovs_core::import::{CsvOptions, import_csv};
use ovs_core::sql::paging::{PageSpec, SortSpec};
use ovs_core::sql::{Dialect, split_statements_for, templates};
use ovs_core::{ConnectionConfig, Driver, Engine, Error, ObjectKind, QueryOutcome, Session, Value};
use ovs_postgres::PostgresDriver;

const D: Dialect = Dialect::Postgres;

fn config() -> Option<(ConnectionConfig, String)> {
    let spec = std::env::var("OVS_TEST_PG").ok()?;
    let mut parts = spec.splitn(4, ':');
    let host = parts.next()?.to_owned();
    let port = parts.next()?.parse().ok()?;
    let user = parts.next()?.to_owned();
    let password = parts.next().unwrap_or_default().to_owned();
    Some((
        ConnectionConfig {
            engine: Engine::Postgres,
            host,
            port,
            user,
            database: "postgres".into(),
            ..ConnectionConfig::default()
        },
        password,
    ))
}

async fn connect() -> Option<Box<dyn Session>> {
    let (cfg, password) = config()?;
    Some(
        PostgresDriver
            .connect(&cfg, Some(&password))
            .await
            .expect("connect"),
    )
}

/// A fresh schema, made current.
async fn scratch(name: &str) -> Option<(Box<dyn Session>, String)> {
    let mut s = connect().await?;
    let schema = format!("ovs_test_{name}");
    for sql in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
    ] {
        s.execute(&sql, None).await.unwrap();
    }
    s.use_database(&schema).await.unwrap();
    Some((s, schema))
}

async fn cleanup(mut s: Box<dyn Session>, schema: &str) {
    s.execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), None)
        .await
        .unwrap();
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> Vec<QueryOutcome> {
    s.execute(sql, None)
        .await
        .unwrap_or_else(|e| panic!("{e}: {sql}"))
}

async fn rows(s: &mut Box<dyn Session>, sql: &str) -> ovs_core::ResultSet {
    match exec(s, sql).await.into_iter().next() {
        Some(QueryOutcome::Rows(rs)) => rs,
        other => panic!("{other:?}"),
    }
}

const SCHEMA: &str = "
    CREATE TABLE owners (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        name text NOT NULL DEFAULT 'it''s', born date, data bytea, score numeric(6, 2),
        active boolean DEFAULT true, CONSTRAINT name_set CHECK (name <> ''));
    COMMENT ON TABLE owners IS 'people';
    CREATE TABLE pets (owner bigint NOT NULL REFERENCES owners (id) ON DELETE CASCADE,
        tag text NOT NULL, weight double precision,
        half bigint GENERATED ALWAYS AS (owner / 2) STORED, PRIMARY KEY (owner, tag));
    CREATE UNIQUE INDEX pets_weight ON pets (weight, tag);
    CREATE TABLE log (msg text, n integer);
    CREATE VIEW heavy AS SELECT * FROM pets WHERE weight > 10;
    CREATE FUNCTION log_owner() RETURNS trigger LANGUAGE plpgsql AS $$
    BEGIN
      INSERT INTO log VALUES ('owner; added', NEW.id);
      RETURN NEW;
    END;
    $$;
    CREATE TRIGGER owners_log AFTER INSERT ON owners FOR EACH ROW EXECUTE FUNCTION log_owner();
";

/// Runs a script the way the app does: split with PostgreSQL's rules, one statement at a time.
async fn script(s: &mut Box<dyn Session>, sql: &str) {
    for statement in split_statements_for(sql, D) {
        exec(s, &statement.sql).await;
    }
}

#[tokio::test]
async fn queries_carry_types_and_origins() {
    let Some((mut s, schema)) = scratch("queries").await else {
        return;
    };
    assert!(s.server_version().starts_with("PostgreSQL "));
    script(&mut s, SCHEMA).await;
    let out = exec(
        &mut s,
        "INSERT INTO owners (name, data, score) VALUES ('ada', '\\x00ff', 1.5), ('bob', NULL, NULL)",
    )
    .await;
    assert!(matches!(out[0], QueryOutcome::Affected { rows: 2, .. }));

    let rs = rows(
        &mut s,
        "SELECT id, name AS who, data, score, active, 1 + 1 AS two FROM owners ORDER BY id",
    )
    .await;
    assert_eq!(rs.columns[1].origin_name, "name");
    assert_eq!(rs.columns[1].table, "owners");
    assert_eq!(rs.columns[1].schema, schema);
    assert!(!rs.columns[1].nullable && rs.columns[2].binary);
    assert_eq!(rs.columns[5].table, "");
    assert_eq!(rs.rows[0][0], Value::Int(1));
    assert_eq!(rs.rows[0][2], Value::Bytes(vec![0, 0xff]));
    assert_eq!(rs.rows[0][3], Value::Text("1.50".into()));
    assert_eq!(rs.rows[0][4], Value::Text("true".into()));
    assert_eq!(rs.rows[1][5], Value::Int(2));

    // The trigger's function body, with its semicolons, came through whole.
    let log = rows(&mut s, "SELECT msg FROM log ORDER BY n").await;
    assert_eq!(log.rows[0][0], Value::Text("owner; added".into()));

    let limited = s.execute("SELECT * FROM owners", Some(1)).await.unwrap();
    let QueryOutcome::Rows(limited) = &limited[0] else {
        panic!()
    };
    assert!(limited.truncated && limited.rows.len() == 1);

    // Several statements at once: one result each.
    let out = exec(&mut s, "SELECT 1; UPDATE log SET n = n; SELECT 'x'").await;
    assert_eq!(out.len(), 3);
    assert!(matches!(out[1], QueryOutcome::Affected { rows: 2, .. }));

    let err = s
        .execute("SELECT nope FROM owners", None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("42703"), "{err}");

    let mut page = PageSpec {
        base: "SELECT * FROM owners".into(),
        filter: "name <> 'zed'".into(),
        sort: Some(SortSpec {
            column: "name".into(),
            descending: true,
        }),
        offset: 0,
        page_size: 1,
    };
    page.next();
    let rs = rows(&mut s, &page.page_query(D)).await;
    assert_eq!(rs.rows[0][1], Value::Text("ada".into()));
    assert_eq!(
        rows(&mut s, &page.count_query()).await.rows[0][0],
        Value::Int(2)
    );
    cleanup(s, &schema).await;
}

#[tokio::test]
async fn introspects_and_recreates_objects() {
    let Some((mut s, schema)) = scratch("schema").await else {
        return;
    };
    script(&mut s, SCHEMA).await;
    assert!(s.list_databases().await.unwrap().contains(&schema));
    let names = |objects: Vec<ovs_core::SchemaObject>| -> Vec<String> {
        objects.into_iter().map(|o| o.name).collect()
    };
    assert_eq!(
        names(s.list_objects(&schema, ObjectKind::Table).await.unwrap()),
        ["log", "owners", "pets"]
    );
    assert_eq!(
        names(s.list_objects(&schema, ObjectKind::View).await.unwrap()),
        ["heavy"]
    );
    assert_eq!(
        names(s.list_objects(&schema, ObjectKind::Function).await.unwrap()),
        ["log_owner"]
    );
    let triggers = s.list_objects(&schema, ObjectKind::Trigger).await.unwrap();
    assert_eq!(
        (triggers[0].name.as_str(), triggers[0].detail.as_str()),
        ("owners_log", "owners")
    );

    let owners = s.list_columns(&schema, "owners").await.unwrap();
    assert!(owners[0].is_primary() && owners[0].extra == "auto_increment");
    assert_eq!(owners[1].default.as_deref(), Some("it's"));
    assert_eq!(owners[4].column_type, "numeric(6,2)");
    let pets = s.list_columns(&schema, "pets").await.unwrap();
    assert_eq!(pets[3].extra, "STORED GENERATED");

    let indexes = s.list_indexes(&schema, "pets").await.unwrap();
    assert_eq!(indexes[0].name, "PRIMARY");
    assert_eq!(indexes[0].columns, ["owner", "tag"]);
    assert!(indexes.iter().any(|i| i.name == "pets_weight" && i.unique));
    let fks = s.list_foreign_keys(&schema, "pets").await.unwrap();
    assert_eq!(fks[0].columns, ["owner"]);
    assert_eq!(
        (fks[0].ref_table.as_str(), fks[0].on_delete.as_str()),
        ("owners", "CASCADE")
    );
    let checks = s.list_checks(&schema, "owners").await.unwrap();
    assert_eq!(checks[0].name, "name_set");
    assert!(checks[0].clause.contains("<> ''"), "{checks:?}");
    assert_eq!(
        s.table_options(&schema, "owners").await.unwrap().comment,
        "people"
    );

    // Every object's DDL re-creates it in another schema.
    let copy = format!("{schema}_copy");
    exec(&mut s, &format!("DROP SCHEMA IF EXISTS {copy} CASCADE")).await;
    exec(&mut s, &format!("CREATE SCHEMA {copy}")).await;
    let mut ddl = Vec::new();
    for (kind, name) in [
        (ObjectKind::Table, "owners"),
        (ObjectKind::Table, "pets"),
        (ObjectKind::Table, "log"),
        (ObjectKind::View, "heavy"),
        (ObjectKind::Function, "log_owner"),
        (ObjectKind::Trigger, "owners_log"),
    ] {
        let sql = s.object_ddl(&schema, name, kind).await.unwrap();
        ddl.push(
            sql.replace(&format!("\"{schema}\"."), &format!("\"{copy}\"."))
                .replace(&format!("{schema}."), &format!("{copy}.")),
        );
    }
    assert!(
        ddl[0].contains("GENERATED ALWAYS AS IDENTITY"),
        "{}",
        ddl[0]
    );
    assert!(
        ddl[1].contains("GENERATED ALWAYS AS ((owner / 2)) STORED"),
        "{}",
        ddl[1]
    );
    s.use_database(&copy).await.unwrap();
    for sql in &ddl {
        script(&mut s, &format!("{sql};")).await;
    }
    exec(&mut s, "INSERT INTO owners (name) VALUES ('copied')").await;
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM log").await.rows[0][0],
        Value::Int(1)
    );
    exec(&mut s, &format!("DROP SCHEMA {copy} CASCADE")).await;
    cleanup(s, &schema).await;
}

#[tokio::test]
async fn grid_edits_imports_and_templates_run() {
    let Some((mut s, schema)) = scratch("edits").await else {
        return;
    };
    script(&mut s, SCHEMA).await;
    exec(
        &mut s,
        "INSERT INTO log VALUES ('dup', 1), ('dup', 1), ('x', NULL)",
    )
    .await;

    // Keyless: one of two identical rows changes, found by its ctid.
    let rs = rows(&mut s, "SELECT msg, n FROM log ORDER BY msg").await;
    let target = EditTarget::detect(&rs, &[], &[]).unwrap();
    let mut edits = PendingEdits::default();
    edits.set_cell(0, 1, Value::Int(5), &rs.rows[0][1]);
    edits.toggle_delete(2);
    for sql in edits.statements(&target, &rs, D) {
        exec(&mut s, &sql).await;
    }
    let after = rows(&mut s, "SELECT msg, n FROM log ORDER BY n").await;
    assert_eq!(
        after.rows,
        [
            vec![Value::Text("dup".into()), Value::Int(1)],
            vec![Value::Text("dup".into()), Value::Int(5)],
        ]
    );

    // With a key: update the generated-identity table by id.
    exec(&mut s, "INSERT INTO owners (name) VALUES ('ada')").await;
    let rs = rows(&mut s, "SELECT * FROM owners").await;
    let columns = s.list_columns(&schema, "owners").await.unwrap();
    let indexes = s.list_indexes(&schema, "owners").await.unwrap();
    let target = EditTarget::detect(&rs, &columns, &indexes).unwrap();
    let mut edits = PendingEdits::default();
    edits.set_cell(0, 1, Value::Text("lovelace".into()), &rs.rows[0][1]);
    for sql in edits.statements(&target, &rs, D) {
        exec(&mut s, &sql).await;
    }
    assert_eq!(
        rows(&mut s, "SELECT name FROM owners").await.rows[0][0],
        Value::Text("lovelace".into())
    );

    let csv = "msg,n\nfrom csv,10\n\"with, comma\",11\n";
    let (stats, _) = import_csv(s.as_mut(), &schema, "log", csv, &CsvOptions::default())
        .await
        .unwrap();
    assert_eq!(stats.rows, 2);
    let bad = "msg,n\nok,12\nbad,not-a-number\n";
    assert!(
        import_csv(s.as_mut(), &schema, "log", bad, &CsvOptions::default())
            .await
            .is_err()
    );
    // Two left after the edits, one from the owners trigger, two imported.
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM log").await.rows[0][0],
        Value::Int(5)
    );

    // Every template and info query runs.
    for kind in [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Procedure,
        ObjectKind::Function,
        ObjectKind::Trigger,
    ] {
        script(
            &mut s,
            &templates::create_template(D, kind, &schema, Some("new_table")),
        )
        .await;
    }
    script(&mut s, &templates::table_info(D, &schema, "new_table")).await;
    script(&mut s, &templates::database_info(D, &schema)).await;
    for query in templates::server_queries(D) {
        script(&mut s, query).await;
    }
    exec(&mut s, &templates::truncate_table(D, &schema, "new_table")).await;
    exec(
        &mut s,
        &templates::rename_table(D, &schema, "new_table", "renamed"),
    )
    .await;
    exec(
        &mut s,
        &templates::drop_object(D, ObjectKind::Table, &schema, "renamed"),
    )
    .await;
    cleanup(s, &schema).await;
}

#[tokio::test]
async fn long_queries_can_be_cancelled() {
    let Some(mut s) = connect().await else {
        return;
    };
    let cancel = s.cancel_handle();
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    let err = s.execute("SELECT pg_sleep(30)", None).await.unwrap_err();
    stopper.await.unwrap();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert_eq!(rows(&mut s, "SELECT 7").await.rows[0][0], Value::Int(7));
}

/// Needs a server with SSL on and a self-signed certificate (the image's snakeoil one), and
/// `OVS_TEST_PG_TLS=1`:
///
/// ```sh
/// docker run -d --name ovs-postgres -e POSTGRES_PASSWORD=secret -p 35432:5432 postgres:17 \
///   -c ssl=on -c ssl_cert_file=/etc/ssl/certs/ssl-cert-snakeoil.pem \
///   -c ssl_key_file=/etc/ssl/private/ssl-cert-snakeoil.key
/// ```
#[tokio::test]
async fn tls_encrypts_and_verifies() {
    if std::env::var("OVS_TEST_PG_TLS").is_err() {
        return;
    }
    let Some((cfg, password)) = config() else {
        return;
    };
    let encrypted = ConnectionConfig {
        ssl: ovs_core::SslMode::Preferred,
        ..cfg.clone()
    };
    let mut s = PostgresDriver
        .connect(&encrypted, Some(&password))
        .await
        .unwrap();
    let rs = rows(
        &mut s,
        "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    )
    .await;
    assert_eq!(rs.rows[0][0], Value::Text("true".into()));
    // Cancelling goes over TLS too.
    let cancel = s.cancel_handle();
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await.unwrap();
    });
    assert!(matches!(
        s.execute("SELECT pg_sleep(30)", None).await,
        Err(Error::Cancelled)
    ));
    stopper.await.unwrap();

    // A self-signed certificate doesn't verify.
    let verified = ConnectionConfig {
        ssl: ovs_core::SslMode::Required,
        ..cfg
    };
    let Err(err) = PostgresDriver.connect(&verified, Some(&password)).await else {
        panic!("connected with an untrusted certificate");
    };
    assert!(
        err.to_string().to_lowercase().contains("certificate"),
        "{err}"
    );
}
