//! The SQLite driver against real database files in the temp directory.

use std::path::PathBuf;
use std::time::Duration;

use ovs_core::editing::{EditTarget, PendingEdits};
use ovs_core::import::{CsvOptions, import_csv};
use ovs_core::sql::paging::PageSpec;
use ovs_core::{ConnectionConfig, Driver, Engine, Error, ObjectKind, QueryOutcome, Session, Value};
use ovs_sqlite::SqliteDriver;

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ovs-test-{name}-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    fn config(&self) -> ConnectionConfig {
        ConnectionConfig {
            engine: Engine::Sqlite,
            path: self.0.to_string_lossy().into_owned(),
            ..ConnectionConfig::default()
        }
    }

    async fn open(&self) -> Box<dyn Session> {
        SqliteDriver.connect(&self.config(), None).await.unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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
    CREATE TABLE owners (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'it''s', \
        born TEXT, data BLOB);
    CREATE TABLE pets (owner INTEGER NOT NULL REFERENCES owners (id) ON DELETE CASCADE, \
        tag TEXT NOT NULL, weight REAL, half INTEGER GENERATED ALWAYS AS (owner / 2) VIRTUAL, \
        PRIMARY KEY (owner, tag));
    CREATE UNIQUE INDEX pets_weight ON pets (weight, tag);
    CREATE TABLE log (msg TEXT, n INTEGER);
    CREATE VIEW heavy AS SELECT * FROM pets WHERE weight > 10;
    CREATE TRIGGER log_owner AFTER INSERT ON owners BEGIN INSERT INTO log VALUES ('owner', NEW.id); END;
";

#[tokio::test]
async fn runs_queries_with_column_metadata() {
    let db = TempDb::new("queries");
    let mut s = db.open().await;
    assert!(s.server_version().starts_with("SQLite 3."));
    exec(&mut s, SCHEMA).await;
    let out = exec(
        &mut s,
        "INSERT INTO owners (name, data) VALUES ('ada', X'00FF'), ('bob', NULL);",
    )
    .await;
    assert!(matches!(
        out[0],
        QueryOutcome::Affected {
            rows: 2,
            last_insert_id: Some(2),
            ..
        }
    ));

    let rs = rows(
        &mut s,
        "SELECT id, name AS who, data, 1 + 1 AS two FROM owners ORDER BY id",
    )
    .await;
    let names: Vec<&str> = rs.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "who", "data", "two"]);
    assert_eq!(rs.columns[1].origin_name, "name");
    assert_eq!(rs.columns[1].table, "owners");
    assert_eq!(rs.columns[1].schema, "main");
    assert!(rs.columns[0].primary_key && !rs.columns[1].nullable);
    assert!(rs.columns[2].binary);
    assert_eq!(rs.columns[3].table, "");
    assert_eq!(rs.rows[0][2], Value::Bytes(vec![0, 0xff]));
    assert_eq!(rs.rows[1][3], Value::Int(2));

    let limited = s.execute("SELECT * FROM owners", Some(1)).await.unwrap();
    let QueryOutcome::Rows(limited) = &limited[0] else {
        panic!()
    };
    assert!(limited.truncated && limited.rows.len() == 1);

    // The trigger fired once per owner.
    assert_eq!(
        rows(&mut s, "SELECT COUNT(*) FROM log").await.rows[0][0],
        Value::Int(2)
    );

    let err = s
        .execute("SELECT nope FROM owners", None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Server { .. }), "{err:?}");

    // Paging wraps the query like it does for MySQL.
    let mut page = PageSpec {
        base: "SELECT * FROM owners".into(),
        filter: "name <> 'zed'".into(),
        sort: Some(ovs_core::sql::paging::SortSpec {
            column: "name".into(),
            descending: false,
        }),
        offset: 0,
        page_size: 1,
    };
    page.next();
    let sql = page.page_query(s.dialect());
    let rs = rows(&mut s, &sql).await;
    assert_eq!(rs.rows[0][1], Value::Text("bob".into()));
    assert_eq!(
        rows(&mut s, &page.count_query()).await.rows[0][0],
        Value::Int(2)
    );
}

#[tokio::test]
async fn introspects_schema_objects() {
    let db = TempDb::new("schema");
    let mut s = db.open().await;
    exec(&mut s, SCHEMA).await;
    assert_eq!(s.list_databases().await.unwrap(), ["main"]);
    let names = |objects: Vec<ovs_core::SchemaObject>| -> Vec<String> {
        objects.into_iter().map(|o| o.name).collect()
    };
    assert_eq!(
        names(s.list_objects("main", ObjectKind::Table).await.unwrap()),
        ["log", "owners", "pets"]
    );
    assert_eq!(
        names(s.list_objects("main", ObjectKind::View).await.unwrap()),
        ["heavy"]
    );
    let triggers = s.list_objects("main", ObjectKind::Trigger).await.unwrap();
    assert_eq!(triggers[0].detail, "owners");
    assert_eq!(
        s.list_objects("main", ObjectKind::Procedure)
            .await
            .unwrap()
            .len(),
        0
    );

    let owners = s.list_columns("main", "owners").await.unwrap();
    assert!(owners[0].is_primary() && owners[0].extra == "auto_increment");
    assert_eq!(owners[1].default.as_deref(), Some("it's"));
    assert!(!owners[1].nullable && owners[2].nullable);
    let pets = s.list_columns("main", "pets").await.unwrap();
    assert_eq!(pets[3].extra, "VIRTUAL GENERATED");
    assert!(pets[0].is_primary() && pets[0].extra.is_empty());

    let indexes = s.list_indexes("main", "pets").await.unwrap();
    assert_eq!(indexes[0].name, "PRIMARY");
    assert_eq!(indexes[0].columns, ["owner", "tag"]);
    assert_eq!(indexes[1].name, "pets_weight");
    assert!(indexes[1].unique && indexes[1].columns == ["weight", "tag"]);
    assert_eq!(indexes.len(), 2);

    let fks = s.list_foreign_keys("main", "pets").await.unwrap();
    assert_eq!(fks.len(), 1);
    assert_eq!(
        (fks[0].ref_table.as_str(), fks[0].on_delete.as_str()),
        ("owners", "CASCADE")
    );
    assert_eq!(
        (fks[0].columns.clone(), fks[0].ref_columns.clone()),
        (vec!["owner".to_owned()], vec!["id".to_owned()])
    );

    let ddl = s
        .object_ddl("main", "heavy", ObjectKind::View)
        .await
        .unwrap();
    assert!(ddl.starts_with("CREATE VIEW heavy"), "{ddl}");
    assert!(
        s.object_ddl("main", "x", ObjectKind::Procedure)
            .await
            .is_err()
    );

    s.use_database("main").await.unwrap();
    assert!(s.use_database("other").await.is_err());
    exec(&mut s, "ATTACH DATABASE ':memory:' AS other").await;
    s.use_database("other").await.unwrap();
    assert_eq!(
        s.current_database().await.unwrap().as_deref(),
        Some("other")
    );
}

#[tokio::test]
async fn grid_edits_and_csv_import_apply() {
    let db = TempDb::new("edits");
    let mut s = db.open().await;
    exec(&mut s, SCHEMA).await;
    exec(
        &mut s,
        "INSERT INTO log VALUES ('dup', 1), ('dup', 1), ('x', NULL)",
    )
    .await;

    // A keyless table: one of two identical rows is changed.
    let rs = rows(&mut s, "SELECT msg, n FROM log ORDER BY rowid").await;
    let target = EditTarget::detect(&rs, &[], &[]).unwrap();
    let mut edits = PendingEdits::default();
    edits.set_cell(0, 1, Value::Int(5), &rs.rows[0][1]);
    edits.toggle_delete(2);
    for sql in edits.statements(&target, &rs, s.dialect()) {
        exec(&mut s, &sql).await;
    }
    let after = rows(&mut s, "SELECT msg, n FROM log ORDER BY rowid").await;
    assert_eq!(
        after.rows,
        [
            vec![Value::Text("dup".into()), Value::Int(5)],
            vec![Value::Text("dup".into()), Value::Int(1)],
        ]
    );

    let csv = "id,name\n10,ada\n11,\"lovelace, a\"\n";
    let (stats, _) = import_csv(s.as_mut(), "main", "owners", csv, &CsvOptions::default())
        .await
        .unwrap();
    assert_eq!(stats.rows, 2);
    // A failing row rolls the whole import back.
    let bad = "id,name\n12,ok\n10,duplicate\n";
    assert!(
        import_csv(s.as_mut(), "main", "owners", bad, &CsvOptions::default())
            .await
            .is_err()
    );
    assert_eq!(
        rows(&mut s, "SELECT COUNT(*) FROM owners").await.rows[0][0],
        Value::Int(2)
    );
}

#[tokio::test]
async fn long_queries_can_be_cancelled() {
    let db = TempDb::new("cancel");
    let mut s = db.open().await;
    let cancel = s.cancel_handle();
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel().await.unwrap();
    });
    let err = s
        .execute(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
             SELECT COUNT(*) FROM c",
            None,
        )
        .await
        .unwrap_err();
    stopper.await.unwrap();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    // The session is still usable afterwards.
    assert_eq!(rows(&mut s, "SELECT 7").await.rows[0][0], Value::Int(7));
}

#[tokio::test]
async fn missing_path_is_reported() {
    let config = ConnectionConfig {
        engine: Engine::Sqlite,
        ..ConnectionConfig::default()
    };
    let Err(err) = SqliteDriver.connect(&config, None).await else {
        panic!("connected without a file");
    };
    assert!(err.to_string().contains("database file"), "{err}");
}

#[tokio::test]
async fn dumps_restores_and_copies_databases() {
    use ovs_core::dump::{DumpOptions, copy_database, dump_database, run_script};

    let src_db = TempDb::new("dump-src");
    let mut src = src_db.open().await;
    exec(&mut src, SCHEMA).await;
    exec(
        &mut src,
        "INSERT INTO owners (name, data) VALUES ('ada', X'00FF'), ('it''s', NULL);
         INSERT INTO pets (owner, tag, weight) VALUES (1, 'rex', 12.5), (2, 'tom', 3);",
    )
    .await;

    let mut out = Vec::new();
    let stats = dump_database(
        src.as_mut(),
        "main",
        &DumpOptions::default(),
        &mut out,
        &mut |_| {},
    )
    .await
    .unwrap();
    // owners and pets, plus the two log rows the trigger wrote.
    assert_eq!((stats.tables, stats.rows), (3, 6));
    let script = String::from_utf8(out).unwrap();
    assert!(!script.contains("DELIMITER"), "{script}");
    assert!(!script.contains("SET NAMES"), "{script}");

    let restored_db = TempDb::new("dump-dst");
    let mut restored = restored_db.open().await;
    let run = run_script(restored.as_mut(), &script, true, &mut |_, _| {}).await;
    assert!(run.errors.is_empty(), "{:?}", run.errors);
    let check = "SELECT (SELECT group_concat(x, ',') FROM (SELECT name || hex(coalesce(data, '')) AS x FROM owners ORDER BY id)) || '|' || \
                 (SELECT group_concat(x, ',') FROM (SELECT tag || weight AS x FROM pets ORDER BY tag)) || '|' || \
                 (SELECT COUNT(*) FROM log) || '|' || \
                 (SELECT COUNT(*) FROM sqlite_master WHERE type IN ('view', 'trigger'))";
    let expected = rows(&mut src, check).await.rows[0][0].clone();
    assert_eq!(rows(&mut restored, check).await.rows[0][0], expected);
    // The restored trigger works.
    exec(&mut restored, "INSERT INTO owners (name) VALUES ('new')").await;
    assert_eq!(
        rows(&mut restored, "SELECT COUNT(*) FROM log").await.rows[0][0],
        Value::Int(3)
    );

    // Copy just the pets table into a third file.
    let copy_db = TempDb::new("copy-dst");
    let mut copy = copy_db.open().await;
    let options = DumpOptions {
        tables: Some(vec!["pets".into()]),
        ..DumpOptions::default()
    };
    let stats = copy_database(
        src.as_mut(),
        "main",
        copy.as_mut(),
        "main",
        &options,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(stats.tables, 1);
    assert_eq!(
        rows(&mut copy, "SELECT COUNT(*) FROM pets").await.rows[0][0],
        Value::Int(2)
    );
    // The trigger belongs to owners, which wasn't copied.
    assert_eq!(
        rows(
            &mut copy,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger'"
        )
        .await
        .rows[0][0],
        Value::Int(0)
    );
}
