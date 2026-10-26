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
                 (SELECT COUNT(*) FROM sqlite_master WHERE type IN ('view', 'trigger', 'index'))";
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

#[tokio::test]
async fn table_designer_rebuilds_tables() {
    use ovs_core::sql::Dialect;
    use ovs_core::sql::design::{
        ColumnDef, IndexDef, IndexKind, TableDesign, alter_sql, create_statements,
    };

    async fn load(s: &mut Box<dyn Session>, table: &str) -> TableDesign {
        let columns = s.list_columns("main", table).await.unwrap();
        let indexes = s.list_indexes("main", table).await.unwrap();
        let fks = s.list_foreign_keys("main", table).await.unwrap();
        let create = s
            .object_ddl("main", table, ObjectKind::Table)
            .await
            .unwrap();
        let mut triggers = Vec::new();
        for t in s.list_objects("main", ObjectKind::Trigger).await.unwrap() {
            if t.detail == table {
                triggers.push(
                    s.object_ddl("main", &t.name, ObjectKind::Trigger)
                        .await
                        .unwrap(),
                );
            }
        }
        TableDesign::from_table(Dialect::Sqlite, table, &columns, &indexes)
            .with_constraints("main", &fks, ovs_core::TableOptions::default())
            .with_sqlite_parts(&create, triggers)
    }

    let db = TempDb::new("designer");
    let mut s = db.open().await;
    let d = s.dialect();
    exec(&mut s, SCHEMA).await;
    exec(
        &mut s,
        "INSERT INTO owners (name) VALUES ('ada'), ('bob');
         INSERT INTO pets (owner, tag, weight) VALUES (1, 'rex', 12.5);",
    )
    .await;

    let before = load(&mut s, "owners").await;
    assert_eq!(before.triggers.len(), 1);
    assert_eq!(alter_sql(d, "main", &before, &before).len(), 0);

    // Rename a column, change a type, drop one, add one and an index.
    let mut after = before.clone();
    after.columns[1].name = "full_name".into();
    after.columns.retain(|c| c.name != "born");
    after.columns[2].data_type = "BLOB".into();
    after.columns.push(ColumnDef {
        name: "score".into(),
        data_type: "INTEGER".into(),
        nullable: false,
        default_sql: "0".into(),
        ..ColumnDef::default()
    });
    after.indexes.push(IndexDef {
        name: "owners_score".into(),
        kind: IndexKind::Index,
        columns: vec!["score".into(), "full_name".into()],
        original_name: None,
    });
    after.validate_for(d).unwrap();
    after.validate_change(d, &before).unwrap();
    for sql in alter_sql(d, "main", &before, &after) {
        exec(&mut s, &sql).await;
    }

    let rs = rows(
        &mut s,
        "SELECT id, full_name, score FROM owners ORDER BY id",
    )
    .await;
    assert_eq!(
        rs.rows,
        [
            vec![Value::Int(1), Value::Text("ada".into()), Value::Int(0)],
            vec![Value::Int(2), Value::Text("bob".into()), Value::Int(0)],
        ]
    );
    let reloaded = load(&mut s, "owners").await;
    let names: Vec<&str> = reloaded.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "full_name", "data", "score"]);
    assert_eq!(reloaded.indexes.len(), 1);
    // The trigger came back and still fires.
    exec(&mut s, "INSERT INTO owners (full_name) VALUES ('cy')").await;
    assert_eq!(
        rows(&mut s, "SELECT COUNT(*) FROM log").await.rows[0][0],
        Value::Int(3)
    );
    // pets still references owners, and nothing broke the link.
    assert_eq!(
        rows(&mut s, "SELECT COUNT(*) FROM pragma_foreign_key_check")
            .await
            .rows[0][0],
        Value::Int(0)
    );
    assert!(
        s.execute("INSERT INTO pets (owner, tag) VALUES (99, 'ghost')", None)
            .await
            .is_err()
    );

    // A failed rebuild leaves the table as it was.
    let mut bad = reloaded.clone();
    bad.columns[1].nullable = false;
    bad.columns.push(ColumnDef {
        name: "must".into(),
        data_type: "TEXT".into(),
        nullable: false,
        ..ColumnDef::default()
    });
    let statements = alter_sql(d, "main", &reloaded, &bad);
    let mut failed = false;
    for sql in &statements {
        if s.execute(sql, None).await.is_err() {
            failed = true;
            for undo in ovs_core::sql::design::recovery_statements(d) {
                let _ = s.execute(undo, None).await;
            }
            break;
        }
    }
    assert!(
        failed,
        "NOT NULL without a default should fail on existing rows"
    );
    assert_eq!(
        rows(&mut s, "SELECT COUNT(*) FROM owners").await.rows[0][0],
        Value::Int(3)
    );

    // New tables, with their indexes.
    let mut new = TableDesign {
        name: "tags".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                data_type: "INTEGER".into(),
                primary_key: true,
                auto_increment: true,
                ..ColumnDef::default()
            },
            ColumnDef {
                name: "label".into(),
                data_type: "TEXT".into(),
                ..ColumnDef::default()
            },
        ],
        ..TableDesign::default()
    };
    new.indexes.push(IndexDef {
        name: "tags_label".into(),
        kind: IndexKind::Unique,
        columns: vec!["label".into()],
        original_name: None,
    });
    for sql in create_statements(d, "main", &new) {
        exec(&mut s, &sql).await;
    }
    exec(&mut s, "INSERT INTO tags (label) VALUES ('a')").await;
    assert!(
        s.execute("INSERT INTO tags (label) VALUES ('a')", None)
            .await
            .is_err()
    );
}

/// Queries from the query builder, joined on the database's own foreign keys, run as written.
#[tokio::test]
async fn query_builder_sql_runs() {
    use ovs_core::sql::builder::{
        Aggregate, ColumnRef, Condition, JoinKind, Operator, OrderItem, Query, SelectItem,
        SelectPart, SetOperator,
    };

    let db = TempDb::new("builder");
    let mut s = db.open().await;
    exec(
        &mut s,
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers (id), total REAL);
         CREATE TABLE suppliers (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO customers VALUES (1, 'ada'), (2, 'bob'), (3, 'cy');
         INSERT INTO orders VALUES (1, 1, 50), (2, 1, 150), (3, 2, 20);
         INSERT INTO suppliers VALUES (1, 'acme'), (2, 'ada');",
    )
    .await;
    let mut fks = std::collections::HashMap::new();
    for table in ["customers", "orders", "suppliers"] {
        fks.insert(table, s.list_foreign_keys("main", table).await.unwrap());
    }
    let lookup = |t: &str| fks.get(t).cloned().unwrap_or_default();

    // Spending per customer, including those with no orders.
    let mut part = SelectPart::default();
    part.add_table("customers", &lookup);
    part.add_table("orders", &lookup);
    assert_eq!(part.joins[0].on.len(), 1, "joined on the foreign key");
    part.joins[0].kind = JoinKind::Left;
    part.columns = vec![
        SelectItem::column(ColumnRef::new("c", "name")),
        SelectItem {
            aggregate: Some(Aggregate::Count),
            alias: "orders".into(),
            ..SelectItem::column(ColumnRef::new("o", "id"))
        },
    ];
    part.order_by = vec![OrderItem {
        column: ColumnRef::new("c", "name"),
        descending: false,
    }];
    let query = Query {
        first: part,
        ..Query::default()
    };
    let sql = query.sql(s.dialect(), "main").unwrap();
    let rs = rows(&mut s, &sql).await;
    assert_eq!(
        rs.rows,
        [
            vec![Value::Text("ada".into()), Value::Int(2)],
            vec![Value::Text("bob".into()), Value::Int(1)],
            vec![Value::Text("cy".into()), Value::Int(0)],
        ]
    );

    // Names of customers with a big order, and of suppliers, without repeats.
    let mut big = SelectPart::default();
    big.add_table("customers", &lookup);
    big.add_table("orders", &lookup);
    big.columns = vec![SelectItem::column(ColumnRef::new("c", "name"))];
    big.conditions = vec![Condition {
        column: ColumnRef::new("o", "total"),
        operator: Operator::Gt,
        value: "100".into(),
    }];
    let mut suppliers = SelectPart::default();
    suppliers.add_table("suppliers", &lookup);
    suppliers.columns = vec![SelectItem::column(ColumnRef::new("s", "name"))];
    let query = Query {
        first: big,
        rest: vec![(SetOperator::Union, suppliers)],
        order_by: vec![(1, false)],
        limit: None,
        offset: None,
    };
    let sql = query.sql(s.dialect(), "main").unwrap();
    let rs = rows(&mut s, &sql).await;
    assert_eq!(
        rs.rows,
        [
            vec![Value::Text("acme".into())],
            vec![Value::Text("ada".into())],
        ]
    );
}

#[tokio::test]
async fn diagram_loads_tables_and_keys() {
    use ovs_core::diagram::Diagram;

    let db = TempDb::new("diagram");
    let mut s = db.open().await;
    exec(&mut s, SCHEMA).await;
    let diagram = Diagram::load(s.as_mut(), "main").await.unwrap();
    let names: Vec<&str> = diagram.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["log", "owners", "pets"]);
    assert_eq!(diagram.relations.len(), 1);
    assert_eq!(
        (
            diagram.relations[0].from_table.as_str(),
            diagram.relations[0].to_table.as_str()
        ),
        ("pets", "owners")
    );
    let pets = diagram.table("pets").unwrap();
    assert!(pets.columns[0].primary && pets.columns[0].foreign);
    // owners is referred to, so it's left of pets.
    assert!(diagram.table("owners").unwrap().x < pets.x);

    let empty = TempDb::new("diagram-empty");
    let mut e = empty.open().await;
    assert!(Diagram::load(e.as_mut(), "main").await.is_err());
}
