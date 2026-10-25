//! Tests against a real server. Skipped unless `OVS_TEST_MYSQL` is set to
//! `host:port:user:password`, e.g.
//!
//! ```sh
//! docker run -d --name ovs-mysql -e MYSQL_ROOT_PASSWORD=secret -p 33306:3306 mysql:8.4
//! OVS_TEST_MYSQL=127.0.0.1:33306:root:secret cargo test -p ovs-mysql --test live
//! ```
//!
//! Each test works in its own scratch database so tests can run in parallel.

use std::time::{Duration, Instant};

use ovs_core::{ConnectionConfig, Driver, Error, ObjectKind, QueryOutcome, Session, Value};
use ovs_mysql::MySqlDriver;

fn config() -> Option<(ConnectionConfig, String)> {
    let spec = std::env::var("OVS_TEST_MYSQL").ok()?;
    let mut parts = spec.splitn(4, ':');
    let host = parts.next()?.to_owned();
    let port = parts.next()?.parse().ok()?;
    let user = parts.next()?.to_owned();
    let password = parts.next().unwrap_or_default().to_owned();
    Some((
        ConnectionConfig {
            host,
            port,
            user,
            ..ConnectionConfig::default()
        },
        password,
    ))
}

/// Connects and creates `ovs_test_<name>`, or returns `None` when no server is configured.
async fn scratch(name: &str) -> Option<(Box<dyn Session>, String)> {
    let (cfg, password) = config()?;
    let mut s = MySqlDriver
        .connect(&cfg, Some(&password))
        .await
        .expect("connect");
    let db = format!("ovs_test_{name}");
    s.execute(&format!("DROP DATABASE IF EXISTS {db}"), None)
        .await
        .unwrap();
    s.execute(&format!("CREATE DATABASE {db}"), None)
        .await
        .unwrap();
    s.use_database(&db).await.unwrap();
    Some((s, db))
}

async fn cleanup(mut s: Box<dyn Session>, db: &str) {
    s.execute(&format!("DROP DATABASE IF EXISTS {db}"), None)
        .await
        .unwrap();
    s.close().await.unwrap();
}

fn rows(outcome: &QueryOutcome) -> &Vec<Vec<Value>> {
    match outcome {
        QueryOutcome::Rows(rs) => &rs.rows,
        QueryOutcome::Affected { .. } => panic!("expected rows, got {outcome:?}"),
    }
}

#[tokio::test]
async fn reports_version_and_types() {
    let Some((mut s, db)) = scratch("types").await else {
        return;
    };
    assert_ne!(s.server_version(), "");

    s.execute(
        "CREATE TABLE t (id INT UNSIGNED PRIMARY KEY, n BIGINT, f DOUBLE, d DECIMAL(10,2), \
         txt VARCHAR(20), bin VARBINARY(4), dt DATETIME, nul INT NULL)",
        None,
    )
    .await
    .unwrap();
    s.execute(
        "INSERT INTO t VALUES (1, -5, 1.5, 3.25, 'héllo', 0x00FF, '2024-01-02 03:04:05', NULL)",
        None,
    )
    .await
    .unwrap();

    let out = s.execute("SELECT * FROM t", None).await.unwrap();
    let QueryOutcome::Rows(rs) = &out[0] else {
        panic!("expected rows")
    };
    assert!(rs.columns[0].primary_key);
    assert_eq!(rs.columns[0].type_name, "INT UNSIGNED");
    assert_eq!(rs.columns[0].table, "t");
    assert!(rs.columns[5].binary);
    assert_eq!(
        rs.rows[0],
        [
            Value::UInt(1),
            Value::Int(-5),
            Value::Float(1.5),
            Value::Text("3.25".into()),
            Value::Text("héllo".into()),
            Value::Bytes(vec![0, 255]),
            Value::Text("2024-01-02 03:04:05".into()),
            Value::Null,
        ]
    );
    cleanup(s, &db).await;
}

#[tokio::test]
async fn reports_affected_rows_and_row_limit() {
    let Some((mut s, db)) = scratch("dml").await else {
        return;
    };
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, v INT)",
        None,
    )
    .await
    .unwrap();
    let out = s
        .execute("INSERT INTO t (v) VALUES (1), (2), (3)", None)
        .await
        .unwrap();
    assert!(matches!(
        out[0],
        QueryOutcome::Affected {
            rows: 3,
            last_insert_id: Some(1),
            ..
        }
    ));

    let out = s.execute("SELECT * FROM t", Some(2)).await.unwrap();
    let QueryOutcome::Rows(rs) = &out[0] else {
        panic!("expected rows")
    };
    assert_eq!(rs.rows.len(), 2);
    assert!(rs.truncated);

    // The connection must still be usable after a truncated read.
    let out = s.execute("SELECT COUNT(*) FROM t", None).await.unwrap();
    assert_eq!(rows(&out[0])[0][0], Value::Int(3));
    cleanup(s, &db).await;
}

#[tokio::test]
async fn returns_every_result_of_a_procedure() {
    let Some((mut s, db)) = scratch("proc").await else {
        return;
    };
    s.execute(
        "CREATE PROCEDURE p() BEGIN SELECT 1 AS a; SELECT 2 AS b, 3 AS c; END",
        None,
    )
    .await
    .unwrap();
    let out = s.execute("CALL p()", None).await.unwrap();
    let sets: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            QueryOutcome::Rows(rs) => Some(rs),
            QueryOutcome::Affected { .. } => None,
        })
        .collect();
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[1].columns.len(), 2);
    cleanup(s, &db).await;
}

#[tokio::test]
async fn introspects_schema() {
    let Some((mut s, db)) = scratch("schema").await else {
        return;
    };
    for sql in [
        "CREATE TABLE parent (id INT PRIMARY KEY, name VARCHAR(10) NOT NULL, UNIQUE KEY uq_name (name)) COMMENT='people'",
        "CREATE VIEW v AS SELECT id FROM parent",
        "CREATE FUNCTION f() RETURNS INT DETERMINISTIC RETURN 1",
        "CREATE TRIGGER trg BEFORE INSERT ON parent FOR EACH ROW SET NEW.name = UPPER(NEW.name)",
    ] {
        s.execute(sql, None).await.unwrap();
    }

    assert!(s.list_databases().await.unwrap().contains(&db));
    let tables = s.list_objects(&db, ObjectKind::Table).await.unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "parent");
    assert_eq!(tables[0].detail, "people");
    assert_eq!(
        s.list_objects(&db, ObjectKind::View).await.unwrap()[0].name,
        "v"
    );
    assert_eq!(
        s.list_objects(&db, ObjectKind::Function).await.unwrap()[0].name,
        "f"
    );
    let triggers = s.list_objects(&db, ObjectKind::Trigger).await.unwrap();
    assert_eq!(triggers[0].detail, "parent");

    let cols = s.list_columns(&db, "parent").await.unwrap();
    assert_eq!(cols.len(), 2);
    assert!(cols[0].is_primary());
    assert!(!cols[1].nullable);

    let idx = s.list_indexes(&db, "parent").await.unwrap();
    assert!(idx.iter().any(ovs_core::IndexInfo::is_primary));
    assert!(idx.iter().any(|i| i.name == "uq_name" && i.unique));

    for (name, kind) in [
        ("parent", ObjectKind::Table),
        ("v", ObjectKind::View),
        ("f", ObjectKind::Function),
        ("trg", ObjectKind::Trigger),
    ] {
        let ddl = s.object_ddl(&db, name, kind).await.unwrap();
        assert!(ddl.to_uppercase().starts_with("CREATE"), "{kind:?}: {ddl}");
    }
    cleanup(s, &db).await;
}

#[tokio::test]
async fn server_errors_carry_codes() {
    let Some((mut s, db)) = scratch("errors").await else {
        return;
    };
    let err = s
        .execute("SELECT * FROM missing_table", None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Server {
                code: Some(1146),
                ..
            }
        ),
        "{err}"
    );
    // Still usable afterwards.
    s.ping().await.unwrap();
    cleanup(s, &db).await;
}

#[tokio::test]
async fn cancel_interrupts_a_running_query() {
    let Some((mut s, db)) = scratch("cancel").await else {
        return;
    };
    let cancel = s.cancel_handle();
    let killer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await
    });
    let started = Instant::now();
    let result = s.execute("SELECT SLEEP(10)", None).await;
    killer.await.unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    // MySQL reports a killed SLEEP as returning 1 rather than an error; either is fine as long
    // as it returned early and the session still works.
    if let Err(err) = result {
        assert!(matches!(err, Error::Cancelled), "{err}");
    }
    s.ping().await.unwrap();
    cleanup(s, &db).await;
}

#[tokio::test]
async fn grid_edits_round_trip_through_real_metadata() {
    use ovs_core::editing::{EditTarget, KeyPlan, PendingEdits};
    use ovs_core::sql::Dialect;

    let Some((mut s, db)) = scratch("grid_edit").await else {
        return;
    };
    s.execute(
        "CREATE TABLE people (id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR(20) NOT NULL, \
         note TEXT NULL)",
        None,
    )
    .await
    .unwrap();
    s.execute(
        "INSERT INTO people (name, note) VALUES ('ada', 'x'), ('alan', NULL)",
        None,
    )
    .await
    .unwrap();

    // Aliased columns still map back to their table columns via the wire metadata.
    let out = s
        .execute(
            &format!("SELECT id, name AS who, note, 1 + 1 AS two FROM {db}.people"),
            None,
        )
        .await
        .unwrap();
    let QueryOutcome::Rows(rs) = &out[0] else {
        panic!("expected rows")
    };
    let cols = s.list_columns(&db, "people").await.unwrap();
    let idx = s.list_indexes(&db, "people").await.unwrap();
    let target = EditTarget::detect(rs, &cols, &idx).expect("editable");
    assert_eq!(target.key, KeyPlan::Unique(vec![0]));
    assert!(target.is_column_editable(1));
    assert!(!target.is_column_editable(3));

    let mut edits = PendingEdits::default();
    edits.set_cell(0, 1, Value::Text("Ada L.".into()), &rs.rows[0][1]);
    edits.set_cell(1, 2, Value::Text("it's fine".into()), &rs.rows[1][2]);
    let new = edits.add_row(4);
    edits.set_inserted_cell(new, 1, Some(Value::Text("grace".into())));
    for sql in edits.statements(&target, rs, Dialect::MySql) {
        s.execute(&sql, None).await.unwrap();
    }

    let out = s
        .execute("SELECT name, note FROM people ORDER BY id", None)
        .await
        .unwrap();
    assert_eq!(
        rows(&out[0]),
        &vec![
            vec![Value::Text("Ada L.".into()), Value::Text("x".into())],
            vec![Value::Text("alan".into()), Value::Text("it's fine".into())],
            vec![Value::Text("grace".into()), Value::Null],
        ]
    );
    cleanup(s, &db).await;
}
