//! The SQLite driver against an existing database file someone else made. Skipped unless
//! `OVS_TEST_SQLITE_FILE` names one; the tests work on a copy, so the file is never changed.
//!
//! Written against the Chinook sample (bracket-quoted identifiers, AUTOINCREMENT keys and a
//! self-referencing foreign key), but only the checks marked as such assume its tables.

use std::path::PathBuf;

use ovs_core::editing::{EditTarget, PendingEdits};
use ovs_core::{ConnectionConfig, Driver, Engine, ObjectKind, QueryOutcome, Session, Value};
use ovs_sqlite::SqliteDriver;

struct Copy(PathBuf);

impl Copy {
    fn of_sample(name: &str) -> Option<Self> {
        let source = std::env::var("OVS_TEST_SQLITE_FILE").ok()?;
        let path =
            std::env::temp_dir().join(format!("ovs-sample-{name}-{}.sqlite", std::process::id()));
        std::fs::copy(&source, &path).unwrap_or_else(|e| panic!("copy {source}: {e}"));
        Some(Self(path))
    }

    fn empty(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ovs-sample-{name}-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    async fn open(&self) -> Box<dyn Session> {
        let config = ConnectionConfig {
            engine: Engine::Sqlite,
            path: self.0.to_string_lossy().into_owned(),
            ..ConnectionConfig::default()
        };
        SqliteDriver.connect(&config, None).await.unwrap()
    }
}

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn rows(s: &mut Box<dyn Session>, sql: &str) -> ovs_core::ResultSet {
    match s
        .execute(sql, None)
        .await
        .unwrap_or_else(|e| panic!("{e}: {sql}"))
        .into_iter()
        .next()
    {
        Some(QueryOutcome::Rows(rs)) => rs,
        other => panic!("{other:?}"),
    }
}

async fn count(s: &mut Box<dyn Session>, sql: &str) -> i64 {
    match rows(s, sql).await.rows[0][0] {
        Value::Int(n) => n,
        ref other => panic!("{other:?}"),
    }
}

/// Every table's name and row count.
async fn table_counts(s: &mut Box<dyn Session>) -> Vec<(String, i64)> {
    let mut counts = Vec::new();
    for table in s.list_objects("main", ObjectKind::Table).await.unwrap() {
        let n = count(
            s,
            &format!(
                "SELECT COUNT(*) FROM \"{}\"",
                table.name.replace('"', "\"\"")
            ),
        )
        .await;
        counts.push((table.name, n));
    }
    counts
}

#[tokio::test]
async fn introspects_every_object() {
    let Some(db) = Copy::of_sample("introspect") else {
        return;
    };
    let mut s = db.open().await;
    let tables = s.list_objects("main", ObjectKind::Table).await.unwrap();
    assert_ne!(tables.len(), 0);
    assert!(
        tables.iter().all(|t| !t.name.starts_with("sqlite_")),
        "internal tables are hidden"
    );
    for table in &tables {
        let columns = s.list_columns("main", &table.name).await.unwrap();
        assert!(!columns.is_empty(), "{}", table.name);
        s.list_indexes("main", &table.name).await.unwrap();
        for fk in s.list_foreign_keys("main", &table.name).await.unwrap() {
            assert!(
                tables.iter().any(|t| t.name == fk.ref_table),
                "{} references {}",
                table.name,
                fk.ref_table
            );
            assert_eq!(fk.columns.len(), fk.ref_columns.len());
        }
        let ddl = s
            .object_ddl("main", &table.name, ObjectKind::Table)
            .await
            .unwrap();
        assert!(ddl.starts_with("CREATE TABLE"), "{ddl}");
    }

    // Chinook: bracket-quoted names come back bare, AUTOINCREMENT keys are auto columns.
    if tables.iter().any(|t| t.name == "employees") {
        let columns = s.list_columns("main", "employees").await.unwrap();
        assert_eq!(columns[0].name, "EmployeeId");
        assert!(columns[0].is_primary() && columns[0].extra == "auto_increment");
        let fks = s.list_foreign_keys("main", "employees").await.unwrap();
        assert_eq!(fks[0].ref_table, "employees");
        assert_eq!(fks[0].columns, ["ReportsTo"]);
    }
}

#[tokio::test]
async fn pages_joins_and_edits() {
    let Some(db) = Copy::of_sample("edits") else {
        return;
    };
    let mut s = db.open().await;
    let tables = s.list_objects("main", ObjectKind::Table).await.unwrap();
    if !tables.iter().any(|t| t.name == "tracks") {
        return;
    }
    let page = rows(
        &mut s,
        "SELECT t.TrackId, t.Name, a.Title, ar.Name AS Artist FROM tracks t \
         JOIN albums a ON a.AlbumId = t.AlbumId JOIN artists ar ON ar.ArtistId = a.ArtistId \
         ORDER BY t.TrackId LIMIT 50 OFFSET 50",
    )
    .await;
    assert_eq!(page.rows.len(), 50);
    assert_eq!(page.rows[0][0], Value::Int(51));

    // Edits through the grid are keyed on the primary key.
    let rs = rows(&mut s, "SELECT * FROM genres ORDER BY GenreId LIMIT 3").await;
    let columns = s.list_columns("main", "genres").await.unwrap();
    let indexes = s.list_indexes("main", "genres").await.unwrap();
    let target = EditTarget::detect(&rs, &columns, &indexes).unwrap();
    let mut edits = PendingEdits::default();
    edits.set_cell(1, 1, Value::Text("Rock 'n' Roll".into()), &rs.rows[1][1]);
    let statements = edits.statements(&target, &rs, s.dialect());
    assert!(statements[0].contains("\"GenreId\""), "{statements:?}");
    for sql in statements {
        s.execute(&sql, None).await.unwrap();
    }
    let after = rows(&mut s, "SELECT Name FROM genres ORDER BY GenreId LIMIT 3").await;
    assert_eq!(after.rows[1][0], Value::Text("Rock 'n' Roll".into()));
    assert_eq!(after.rows[0][0], rs.rows[0][1]);
}

#[tokio::test]
async fn dump_restores_every_row() {
    use ovs_core::dump::{DumpOptions, dump_database, run_script};

    let Some(db) = Copy::of_sample("dump") else {
        return;
    };
    let mut s = db.open().await;
    let before = table_counts(&mut s).await;

    let mut out = Vec::new();
    let stats = dump_database(
        s.as_mut(),
        "main",
        &DumpOptions::default(),
        &mut out,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(stats.tables, before.len());
    let script = String::from_utf8(out).unwrap();

    let restored_db = Copy::empty("restored");
    let mut restored = restored_db.open().await;
    let run = run_script(restored.as_mut(), &script, true, &mut |_, _| {}).await;
    assert!(run.errors.is_empty(), "{:?}", run.errors);
    assert_eq!(table_counts(&mut restored).await, before);
    assert_eq!(
        count(
            &mut restored,
            "SELECT COUNT(*) FROM pragma_foreign_key_check"
        )
        .await,
        0
    );
    let indexes = "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND sql IS NOT NULL";
    assert_eq!(
        count(&mut restored, indexes).await,
        count(&mut s, indexes).await
    );
}

#[tokio::test]
async fn designer_rebuilds_a_referenced_table() {
    use ovs_core::sql::Dialect;
    use ovs_core::sql::design::{ColumnDef, TableDesign, alter_sql};

    let Some(db) = Copy::of_sample("designer") else {
        return;
    };
    let mut s = db.open().await;
    let tables = s.list_objects("main", ObjectKind::Table).await.unwrap();
    if !tables.iter().any(|t| t.name == "artists") {
        return;
    }
    let d = s.dialect();
    let load = async |s: &mut Box<dyn Session>| {
        let columns = s.list_columns("main", "artists").await.unwrap();
        let indexes = s.list_indexes("main", "artists").await.unwrap();
        let fks = s.list_foreign_keys("main", "artists").await.unwrap();
        let create = s
            .object_ddl("main", "artists", ObjectKind::Table)
            .await
            .unwrap();
        TableDesign::from_table(Dialect::Sqlite, "artists", &columns, &indexes)
            .with_constraints("main", &fks, ovs_core::TableOptions::default())
            .with_sqlite_parts(&create, Vec::new())
    };
    let counts = table_counts(&mut s).await;
    let seq = "SELECT seq FROM sqlite_sequence WHERE name = 'artists'";
    let next_id = count(&mut s, seq).await;

    let before = load(&mut s).await;
    let mut after = before.clone();
    after.columns.push(ColumnDef {
        name: "Country".into(),
        data_type: "NVARCHAR(40)".into(),
        nullable: true,
        ..ColumnDef::default()
    });
    after.validate_for(d).unwrap();
    after.validate_change(d, &before).unwrap();
    for sql in alter_sql(d, "main", &before, &after) {
        s.execute(&sql, None)
            .await
            .unwrap_or_else(|e| panic!("{e}: {sql}"));
    }

    assert_eq!(table_counts(&mut s).await, counts);
    assert_eq!(
        count(&mut s, "SELECT COUNT(*) FROM pragma_foreign_key_check").await,
        0
    );
    // albums still points at the rebuilt table, and the AUTOINCREMENT counter survived.
    let fks = s.list_foreign_keys("main", "albums").await.unwrap();
    assert!(fks.iter().any(|fk| fk.ref_table == "artists"));
    assert_eq!(count(&mut s, seq).await, next_id);
    s.execute(
        "INSERT INTO artists (Name, Country) VALUES ('new', 'CA')",
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        count(&mut s, "SELECT MAX(ArtistId) FROM artists").await,
        next_id + 1
    );
    let reloaded = load(&mut s).await;
    assert_eq!(reloaded.columns.last().unwrap().name, "Country");
    assert!(reloaded.columns[0].auto_increment);
}
