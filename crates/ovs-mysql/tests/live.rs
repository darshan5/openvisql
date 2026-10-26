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

#[tokio::test]
async fn paging_filtering_and_sorting_queries_run() {
    use ovs_core::sql::Dialect;
    use ovs_core::sql::paging::{PageSpec, SortSpec};

    let Some((mut s, db)) = scratch("paging").await else {
        return;
    };
    s.execute(
        "CREATE TABLE n (id INT PRIMARY KEY, parity VARCHAR(4))",
        None,
    )
    .await
    .unwrap();
    let values: Vec<String> = (1..=120)
        .map(|i| format!("({i}, '{}')", if i % 2 == 0 { "even" } else { "odd" }))
        .collect();
    s.execute(&format!("INSERT INTO n VALUES {}", values.join(",")), None)
        .await
        .unwrap();

    let mut spec = PageSpec {
        base: format!("SELECT id, parity FROM {db}.n ORDER BY id"),
        filter: String::new(),
        sort: None,
        offset: 0,
        page_size: 50,
    };
    let first = |out: &[QueryOutcome]| rows(&out[0]).first().map(|r| r[0].clone());
    spec.next();
    let out = s
        .execute(&spec.page_query(Dialect::MySql), None)
        .await
        .unwrap();
    assert_eq!(rows(&out[0]).len(), 50);
    assert_eq!(first(&out), Some(Value::Int(51)));

    spec.offset = 0;
    spec.filter = "parity = 'even'".into();
    spec.sort = Some(SortSpec {
        column: "id".into(),
        descending: true,
    });
    let out = s
        .execute(&spec.page_query(Dialect::MySql), None)
        .await
        .unwrap();
    assert_eq!(first(&out), Some(Value::Int(120)));
    let count = s.execute(&spec.count_query(), None).await.unwrap();
    assert_eq!(rows(&count[0])[0][0], Value::Int(60));
    cleanup(s, &db).await;
}

#[tokio::test]
async fn object_templates_and_info_queries_run() {
    use ovs_core::sql::templates::{
        create_template, database_info, drop_object, rename_table, table_info, truncate_table,
    };
    use ovs_core::sql::{Dialect, split_statements};

    let Some((mut s, db)) = scratch("templates").await else {
        return;
    };
    // The table first: the trigger template needs it.
    for kind in [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Procedure,
        ObjectKind::Function,
        ObjectKind::Trigger,
        ObjectKind::Event,
    ] {
        for stmt in split_statements(&create_template(
            Dialect::MySql,
            kind,
            &db,
            Some("new_table"),
        )) {
            s.execute(&stmt.sql, None)
                .await
                .unwrap_or_else(|e| panic!("{kind:?}: {e}\n{}", stmt.sql));
        }
    }
    for stmt in split_statements(&table_info(Dialect::MySql, &db, "new_table")) {
        s.execute(&stmt.sql, None).await.unwrap();
    }
    for stmt in split_statements(&database_info(Dialect::MySql, &db)) {
        s.execute(&stmt.sql, None).await.unwrap();
    }
    for sql in [
        ovs_core::sql::templates::PROCESS_LIST,
        ovs_core::sql::templates::SERVER_INFO,
        ovs_core::sql::templates::STATUS_VARIABLES,
        ovs_core::sql::templates::SYSTEM_VARIABLES,
    ] {
        for stmt in split_statements(sql) {
            s.execute(&stmt.sql, None)
                .await
                .unwrap_or_else(|e| panic!("{e}\n{}", stmt.sql));
        }
    }
    s.execute(&truncate_table(Dialect::MySql, &db, "new_table"), None)
        .await
        .unwrap();
    s.execute(
        &rename_table(Dialect::MySql, &db, "new_table", "renamed"),
        None,
    )
    .await
    .unwrap();
    s.execute(
        &drop_object(Dialect::MySql, ObjectKind::Trigger, &db, "new_trigger"),
        None,
    )
    .await
    .unwrap();
    s.execute(
        &drop_object(Dialect::MySql, ObjectKind::Table, &db, "renamed"),
        None,
    )
    .await
    .unwrap();
    cleanup(s, &db).await;
}

#[tokio::test]
async fn dump_and_restore_round_trip() {
    use ovs_core::dump::{DumpOptions, dump_database, run_script};

    let Some((mut s, db)) = scratch("dump_src").await else {
        return;
    };
    for sql in [
        "CREATE TABLE digits (d INT PRIMARY KEY)",
        "INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)",
        "CREATE TABLE big (n INT PRIMARY KEY, half INT AS (n DIV 2) VIRTUAL)",
        "INSERT INTO big (n) SELECT a.d + 10*b.d + 100*c.d + 1000*e.d FROM digits a, digits b, digits c, digits e",
        "INSERT INTO big (n) SELECT 10000 + a.d + 10*b.d + 100*c.d FROM digits a, digits b, digits c",
        "CREATE TABLE odd (id INT PRIMARY KEY, txt TEXT NULL, bin VARBINARY(8) NULL, d DECIMAL(10,3))",
        "INSERT INTO odd VALUES (1, 'it''s a \\\\ path\\n€', 0x00FF10, 1.250), (2, NULL, NULL, -0.001)",
        "CREATE TABLE nokey (a INT, b VARCHAR(5))",
        "INSERT INTO nokey VALUES (1, 'x'), (1, 'x'), (NULL, NULL)",
        "CREATE VIEW v_odd AS SELECT id, txt FROM odd",
        "CREATE PROCEDURE p_count() BEGIN SELECT COUNT(*) FROM big; END",
        "CREATE FUNCTION f_double(x INT) RETURNS INT DETERMINISTIC RETURN x * 2",
        "CREATE TRIGGER t_odd BEFORE INSERT ON odd FOR EACH ROW SET NEW.d = COALESCE(NEW.d, 0)",
        "CREATE EVENT e_noop ON SCHEDULE EVERY 1 DAY DISABLE DO SELECT 1",
    ] {
        s.execute(sql, None)
            .await
            .unwrap_or_else(|e| panic!("{e}: {sql}"));
    }

    let mut out = Vec::new();
    let stats = dump_database(
        s.as_mut(),
        &db,
        &DumpOptions::default(),
        &mut out,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(stats.tables, 4);
    assert_eq!(stats.rows, 10 + 11_000 + 2 + 3);
    assert_eq!(stats.other_objects, 5);
    let script = String::from_utf8(out).unwrap();
    assert!(!script.contains("DEFINER="), "definer should be stripped");

    // Restore into a second database and compare.
    let target = format!("{db}_copy");
    s.execute(&format!("DROP DATABASE IF EXISTS {target}"), None)
        .await
        .unwrap();
    s.execute(&format!("CREATE DATABASE {target}"), None)
        .await
        .unwrap();
    s.use_database(&target).await.unwrap();
    let restored = run_script(s.as_mut(), &script, true, &mut |_, _| {}).await;
    assert!(restored.errors.is_empty(), "{:?}", restored.errors);

    // Compare the rows themselves: MariaDB's CHECKSUM TABLE depends on how virtual columns
    // are stored internally, so identical tables can checksum differently.
    for table in ["digits", "big", "odd", "nokey"] {
        let mut copies = Vec::new();
        for schema in [&db, &target] {
            let out = s
                .execute(&format!("SELECT * FROM {schema}.{table}"), None)
                .await
                .unwrap();
            let mut data = rows(&out[0]).clone();
            data.sort_by(|x, y| {
                x.iter()
                    .zip(y)
                    .map(|(a, b)| a.sort_cmp(b))
                    .find(|o| o.is_ne())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            copies.push(data);
        }
        assert_eq!(copies[0], copies[1], "{table} differs");
    }
    for (kind, name) in [
        (ObjectKind::View, "v_odd"),
        (ObjectKind::Procedure, "p_count"),
        (ObjectKind::Function, "f_double"),
        (ObjectKind::Trigger, "t_odd"),
        (ObjectKind::Event, "e_noop"),
    ] {
        let objects = s.list_objects(&target, kind).await.unwrap();
        assert!(
            objects.iter().any(|o| o.name == name),
            "{kind:?} {name} missing"
        );
    }
    s.execute(&format!("DROP DATABASE {target}"), None)
        .await
        .unwrap();
    cleanup(s, &db).await;
}

#[tokio::test]
async fn csv_import_inserts_and_rolls_back_on_error() {
    use ovs_core::import::{CsvOptions, import_csv};

    let Some((mut s, db)) = scratch("csv").await else {
        return;
    };
    s.execute(
        "CREATE TABLE people (id INT PRIMARY KEY, name VARCHAR(40) NOT NULL, born DATE NULL, \
         score DECIMAL(5,2) NULL)",
        None,
    )
    .await
    .unwrap();

    let good =
        "id,name,born,score,ignored\n1,\"Lovelace, Ada\",1815-12-10,9.5,x\n2,Turing,NULL,,y\n";
    let options = CsvOptions {
        empty_is_null: true,
        ..CsvOptions::default()
    };
    let (stats, plan) = import_csv(s.as_mut(), &db, "people", good, &options)
        .await
        .unwrap();
    assert_eq!(stats.rows, 2);
    assert_eq!(plan.skipped, ["ignored"]);
    let out = s
        .execute("SELECT name, born, score FROM people ORDER BY id", None)
        .await
        .unwrap();
    assert_eq!(
        rows(&out[0]),
        &vec![
            vec![
                Value::Text("Lovelace, Ada".into()),
                Value::Text("1815-12-10".into()),
                Value::Text("9.50".into()),
            ],
            vec![Value::Text("Turing".into()), Value::Null, Value::Null],
        ]
    );

    // A duplicate key in the second batch rolls back the first batch too.
    let bad = "id,name\n3,Hopper\n1,Duplicate\n";
    let options = CsvOptions {
        rows_per_insert: 1,
        ..CsvOptions::default()
    };
    let err = import_csv(s.as_mut(), &db, "people", bad, &options)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nothing was imported"), "{err}");
    let out = s
        .execute("SELECT COUNT(*) FROM people", None)
        .await
        .unwrap();
    assert_eq!(rows(&out[0])[0][0], Value::Int(2));
    cleanup(s, &db).await;
}

#[tokio::test]
async fn table_designer_sql_runs() {
    use ovs_core::sql::Dialect;
    use ovs_core::sql::design::{
        ColumnDef, IndexDef, IndexKind, TableDesign, alter_sql, create_sql,
    };

    let Some((mut s, db)) = scratch("designer").await else {
        return;
    };
    let new_table = TableDesign {
        name: "items".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                data_type: "INT UNSIGNED".into(),
                primary_key: true,
                auto_increment: true,
                ..ColumnDef::default()
            },
            ColumnDef {
                name: "label".into(),
                data_type: "VARCHAR(20)".into(),
                nullable: true,
                default_sql: "'it''s new'".into(),
                comment: "shown to users".into(),
                ..ColumnDef::default()
            },
            ColumnDef {
                name: "created".into(),
                data_type: "DATETIME".into(),
                default_sql: "CURRENT_TIMESTAMP".into(),
                ..ColumnDef::default()
            },
            ColumnDef {
                name: "legacy".into(),
                data_type: "INT".into(),
                nullable: true,
                ..ColumnDef::default()
            },
        ],
        indexes: vec![
            IndexDef {
                name: "by_label".into(),
                columns: vec!["label".into()],
                ..IndexDef::default()
            },
            IndexDef {
                name: "by_legacy".into(),
                kind: IndexKind::Unique,
                columns: vec!["legacy".into(), "created".into()],
                ..IndexDef::default()
            },
        ],
        ..TableDesign::default()
    };
    s.execute(&create_sql(Dialect::MySql, &db, &new_table), None)
        .await
        .unwrap();
    s.execute("INSERT INTO items (label) VALUES ('a'), (DEFAULT)", None)
        .await
        .unwrap();

    let load = |cols: Vec<ovs_core::ColumnInfo>, idx: Vec<ovs_core::IndexInfo>| {
        TableDesign::from_table("items", &cols, &idx)
    };
    let before = load(
        s.list_columns(&db, "items").await.unwrap(),
        s.list_indexes(&db, "items").await.unwrap(),
    );
    // Loading an existing table and changing nothing produces no ALTER.
    assert_eq!(
        alter_sql(Dialect::MySql, &db, &before, &before).len(),
        0,
        "{before:?}"
    );

    let mut after = before.clone();
    after.columns[1].name = "title".into();
    after.columns[1].data_type = "VARCHAR(100)".into();
    after.columns.retain(|c| c.name != "legacy");
    after.indexes.retain(|i| i.name != "by_legacy");
    after.follow_renames();
    after.columns.insert(
        1,
        ColumnDef {
            name: "sku".into(),
            data_type: "VARCHAR(10)".into(),
            nullable: true,
            ..ColumnDef::default()
        },
    );
    for sql in alter_sql(Dialect::MySql, &db, &before, &after) {
        s.execute(&sql, None)
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }

    let reloaded = load(
        s.list_columns(&db, "items").await.unwrap(),
        s.list_indexes(&db, "items").await.unwrap(),
    );
    let names: Vec<&str> = reloaded.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "sku", "title", "created"]);
    assert!(
        reloaded.columns[2]
            .data_type
            .to_lowercase()
            .starts_with("varchar(100)")
    );
    assert_eq!(reloaded.columns[2].comment, "shown to users");
    assert_eq!(reloaded.indexes.len(), 1);
    assert_eq!(reloaded.indexes[0].columns, ["title"]);
    // The data survived the rename.
    let out = s
        .execute("SELECT title FROM items ORDER BY id", None)
        .await
        .unwrap();
    assert_eq!(rows(&out[0])[0][0], Value::Text("a".into()));
    assert_eq!(rows(&out[0])[1][0], Value::Text("it's new".into()));

    // Swap the primary key to (sku, id) while id stays AUTO_INCREMENT.
    let mut keyed = reloaded.clone();
    keyed.columns[1].primary_key = true;
    keyed.columns[1].nullable = false;
    s.execute("UPDATE items SET sku = CONCAT('S', id)", None)
        .await
        .unwrap();
    for sql in alter_sql(Dialect::MySql, &db, &reloaded, &keyed) {
        s.execute(&sql, None)
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }
    let idx = s.list_indexes(&db, "items").await.unwrap();
    let pk = idx.iter().find(|i| i.is_primary()).unwrap();
    assert_eq!(pk.columns, ["id", "sku"]);

    // Widen an index and add prefix and full-text ones.
    let current = load(s.list_columns(&db, "items").await.unwrap(), idx);
    let mut indexed = current.clone();
    indexed.indexes[0].columns.push("created".into());
    indexed.indexes.push(IndexDef {
        name: "sku_prefix".into(),
        kind: IndexKind::Unique,
        columns: vec!["sku(5)".into(), "id".into()],
        original_name: None,
    });
    indexed.indexes.push(IndexDef {
        name: "title_text".into(),
        kind: IndexKind::Fulltext,
        columns: vec!["title".into()],
        original_name: None,
    });
    for sql in alter_sql(Dialect::MySql, &db, &current, &indexed) {
        s.execute(&sql, None)
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }
    let final_design = load(
        s.list_columns(&db, "items").await.unwrap(),
        s.list_indexes(&db, "items").await.unwrap(),
    );
    let mut kinds: Vec<(String, IndexKind, usize)> = final_design
        .indexes
        .iter()
        .map(|i| (i.name.clone(), i.kind, i.columns.len()))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        [
            ("by_label".to_owned(), IndexKind::Index, 2),
            ("sku_prefix".to_owned(), IndexKind::Unique, 2),
            ("title_text".to_owned(), IndexKind::Fulltext, 1),
        ]
    );
    cleanup(s, &db).await;
}

#[tokio::test]
async fn table_designer_foreign_keys_and_options() {
    use ovs_core::sql::Dialect;
    use ovs_core::sql::design::{TableDesign, alter_sql};

    async fn load(s: &mut Box<dyn Session>, db: &str) -> TableDesign {
        let columns = s.list_columns(db, "pets").await.unwrap();
        let indexes = s.list_indexes(db, "pets").await.unwrap();
        let fks = s.list_foreign_keys(db, "pets").await.unwrap();
        let options = s.table_options(db, "pets").await.unwrap();
        let checks = s.list_checks(db, "pets").await.unwrap();
        TableDesign::from_table("pets", &columns, &indexes)
            .with_constraints(db, &fks, options)
            .with_checks(&checks)
    }
    let Some((mut s, db)) = scratch("designer_fk").await else {
        return;
    };
    for sql in [
        "CREATE TABLE owners (login VARCHAR(20) NOT NULL PRIMARY KEY, team INT NOT NULL, \
         UNIQUE KEY by_team (team, login)) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
        "CREATE TABLE pets (id INT NOT NULL PRIMARY KEY, owner VARCHAR(20) NULL, \
         team INT NULL, CONSTRAINT fk_owner FOREIGN KEY (owner) REFERENCES owners (login) \
         ON DELETE CASCADE, CONSTRAINT owner_set CHECK (owner <> ''), \
         CONSTRAINT id_positive CHECK (id > 0)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 \
         COLLATE=utf8mb4_unicode_ci COMMENT='animals'",
        "INSERT INTO owners VALUES ('ada', 1)",
        "INSERT INTO pets VALUES (1, 'ada', 1)",
    ] {
        s.execute(sql, None)
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }
    let before = load(&mut s, &db).await;
    assert_eq!(before.options.engine, "InnoDB");
    assert_eq!(before.options.collation, "utf8mb4_unicode_ci");
    assert_eq!(before.options.comment, "animals");
    assert_eq!(before.foreign_keys.len(), 1);
    let fk = &before.foreign_keys[0];
    assert_eq!(
        (fk.ref_database.as_str(), fk.ref_table.as_str()),
        ("", "owners")
    );
    assert_eq!(fk.on_delete, "CASCADE");
    let mut names: Vec<&str> = before.checks.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["id_positive", "owner_set"]);
    assert_eq!(alter_sql(Dialect::MySql, &db, &before, &before).len(), 0);

    // Rename the referencing column, widen the key to two columns and change the options.
    let mut after = before.clone();
    after.columns[1].name = "owner_login".into();
    after.follow_renames();
    let fk = &mut after.foreign_keys[0];
    fk.columns.insert(0, "team".into());
    fk.ref_columns.insert(0, "team".into());
    fk.on_delete = "RESTRICT".into();
    after.options.comment = "pets and owners".into();
    after.options.collation = "utf8mb4_general_ci".into();
    after.validate().unwrap();
    for sql in alter_sql(Dialect::MySql, &db, &before, &after) {
        s.execute(&sql, None)
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }

    let reloaded = load(&mut s, &db).await;
    let fk = &reloaded.foreign_keys[0];
    assert_eq!(fk.columns, ["team", "owner_login"]);
    assert_eq!(fk.ref_columns, ["team", "login"]);
    assert_eq!(fk.on_delete, "RESTRICT");
    assert_eq!(reloaded.options.comment, "pets and owners");
    assert_eq!(reloaded.options.collation, "utf8mb4_general_ci");
    // The check on the renamed column was re-created against its new name.
    let owner_set = reloaded
        .checks
        .iter()
        .find(|c| c.name == "owner_set")
        .unwrap();
    assert!(owner_set.clause.contains("owner_login"), "{owner_set:?}");
    assert!(
        s.execute("INSERT INTO pets (id, owner_login) VALUES (5, '')", None)
            .await
            .is_err()
    );

    // Dropping the foreign key leaves the index the server made for it alone.
    let mut dropped = reloaded.clone();
    dropped.foreign_keys.clear();
    for sql in alter_sql(Dialect::MySql, &db, &reloaded, &dropped) {
        s.execute(&sql, None)
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    }
    assert_eq!(s.list_foreign_keys(&db, "pets").await.unwrap().len(), 0);
    cleanup(s, &db).await;
}

#[tokio::test]
async fn user_manager_statements_run() {
    use ovs_core::sql::Dialect;
    use ovs_core::sql::users::{
        Account, AccountChange, Grants, LIST_ACCOUNTS, Scope, account_statements, drop_user,
        grant_statements, list_database_grants, parse_accounts, parse_privileges, privileges_query,
    };

    const D: Dialect = Dialect::MySql;
    async fn query(s: &mut Box<dyn Session>, sql: &str) -> ovs_core::ResultSet {
        let out = s.execute(sql, None).await.unwrap();
        match out.into_iter().next() {
            Some(QueryOutcome::Rows(rs)) => rs,
            other => panic!("{other:?}"),
        }
    }
    async fn run(s: &mut Box<dyn Session>, statements: Vec<String>) {
        for sql in statements {
            s.execute(&sql, None)
                .await
                .unwrap_or_else(|e| panic!("{e}\n{sql}"));
        }
    }
    let Some((mut s, db)) = scratch("users").await else {
        return;
    };
    let account = Account {
        user: "ovs_test_user".into(),
        host: "10.0.0.%".into(),
    };
    let _ = s.execute(&drop_user(D, &account), None).await;
    run(
        &mut s,
        account_statements(
            D,
            &AccountChange {
                original: None,
                account: account.clone(),
                password: Some("first-Pass1".into()),
            },
        ),
    )
    .await;
    assert!(parse_accounts(&query(&mut s, LIST_ACCOUNTS).await).contains(&account));

    let scope = Scope::Database(db.clone());
    let none = parse_privileges(
        &query(&mut s, &privileges_query(D, &account, &scope)).await,
        &scope,
    );
    assert_eq!(none, Grants::default());
    let wanted = Grants {
        privileges: ["SELECT", "INSERT", "EXECUTE"].map(String::from).into(),
        grant_option: true,
    };
    run(
        &mut s,
        grant_statements(D, &account, &scope, &none, &wanted),
    )
    .await;
    let read = parse_privileges(
        &query(&mut s, &privileges_query(D, &account, &scope)).await,
        &scope,
    );
    assert_eq!(read, wanted);
    let dbs = query(&mut s, &list_database_grants(D, &account)).await;
    assert_eq!(dbs.rows.len(), 1);

    let fewer = Grants {
        privileges: ["SELECT", "UPDATE"].map(String::from).into(),
        grant_option: false,
    };
    run(&mut s, grant_statements(D, &account, &scope, &read, &fewer)).await;
    let read = parse_privileges(
        &query(&mut s, &privileges_query(D, &account, &scope)).await,
        &scope,
    );
    assert_eq!(read, fewer);

    let global_before = parse_privileges(
        &query(&mut s, &privileges_query(D, &account, &Scope::Global)).await,
        &Scope::Global,
    );
    let global_after = Grants {
        privileges: ["PROCESS".to_owned()].into(),
        grant_option: false,
    };
    run(
        &mut s,
        grant_statements(D, &account, &Scope::Global, &global_before, &global_after),
    )
    .await;

    let renamed = Account {
        host: "10.1.%".into(),
        ..account.clone()
    };
    run(
        &mut s,
        account_statements(
            D,
            &AccountChange {
                original: Some(account.clone()),
                account: renamed.clone(),
                password: Some("second-Pass2".into()),
            },
        ),
    )
    .await;
    let global = parse_privileges(
        &query(&mut s, &privileges_query(D, &renamed, &Scope::Global)).await,
        &Scope::Global,
    );
    assert_eq!(global, global_after);
    run(&mut s, vec![drop_user(D, &renamed)]).await;
    assert!(!parse_accounts(&query(&mut s, LIST_ACCOUNTS).await).contains(&renamed));
    cleanup(s, &db).await;
}

#[tokio::test]
async fn copy_database_between_sessions() {
    use ovs_core::dump::{DumpOptions, copy_database};

    let Some((mut src, db)) = scratch("copy_src").await else {
        return;
    };
    for sql in [
        "CREATE TABLE parent (id INT PRIMARY KEY, name VARCHAR(20))",
        "CREATE TABLE child (id INT PRIMARY KEY, parent INT, \
         FOREIGN KEY (parent) REFERENCES parent (id))",
        "CREATE TABLE skipped (id INT PRIMARY KEY)",
        "INSERT INTO parent VALUES (1, 'a'), (2, 'it''s')",
        "INSERT INTO child VALUES (10, 1), (11, 2), (12, 2)",
        "CREATE VIEW v_child AS SELECT c.id, p.name FROM child c JOIN parent p ON p.id = c.parent",
        "CREATE TRIGGER t_skip BEFORE INSERT ON skipped FOR EACH ROW SET NEW.id = NEW.id + 0",
    ] {
        src.execute(sql, None)
            .await
            .unwrap_or_else(|e| panic!("{e}: {sql}"));
    }
    let (cfg, password) = config().unwrap();
    let mut dst = MySqlDriver.connect(&cfg, Some(&password)).await.unwrap();
    let target = format!("{db}_dst");
    dst.execute(&format!("DROP DATABASE IF EXISTS {target}"), None)
        .await
        .unwrap();

    let options = DumpOptions {
        tables: Some(vec!["child".into(), "parent".into()]),
        events: false,
        routines: false,
        ..DumpOptions::default()
    };
    let stats = copy_database(
        src.as_mut(),
        &db,
        dst.as_mut(),
        &target,
        &options,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!((stats.tables, stats.rows, stats.other_objects), (2, 5, 1));

    let tables: Vec<String> = dst
        .list_objects(&target, ObjectKind::Table)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.name)
        .collect();
    assert_eq!(tables, ["child", "parent"]);
    assert_eq!(
        dst.list_objects(&target, ObjectKind::Trigger)
            .await
            .unwrap()
            .len(),
        0
    );
    // With a second server configured, copy across servers too (e.g. MySQL to MariaDB).
    if let Ok(spec) = std::env::var("OVS_TEST_MYSQL_TARGET") {
        let parts: Vec<&str> = spec.splitn(4, ':').collect();
        let other = ConnectionConfig {
            host: parts[0].to_owned(),
            port: parts[1].parse().unwrap(),
            user: parts[2].to_owned(),
            ..cfg.clone()
        };
        let mut far = MySqlDriver
            .connect(&other, parts.get(3).copied())
            .await
            .unwrap();
        let stats = copy_database(
            src.as_mut(),
            &db,
            far.as_mut(),
            &target,
            &DumpOptions::default(),
            &mut |_| {},
        )
        .await
        .unwrap();
        assert_eq!((stats.tables, stats.rows), (3, 5));
        let out = far
            .execute(&format!("SELECT COUNT(*) FROM {target}.v_child"), None)
            .await
            .unwrap();
        assert_eq!(rows(&out[0])[0][0].to_string(), "3");
        far.execute(&format!("DROP DATABASE {target}"), None)
            .await
            .unwrap();
    }
    // The view reads the copied tables, not the source database.
    src.execute(&format!("DROP DATABASE {db}"), None)
        .await
        .unwrap();
    let out = dst
        .execute(
            &format!("SELECT name FROM {target}.v_child ORDER BY id"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(rows(&out[0])[2][0], Value::Text("it's".into()));
    // Recreate the source so cleanup has something to drop.
    src.execute(&format!("CREATE DATABASE {db}"), None)
        .await
        .unwrap();
    dst.execute(&format!("DROP DATABASE {target}"), None)
        .await
        .unwrap();
    cleanup(src, &db).await;
}
