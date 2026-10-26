# Roadmap

Behaviour targets come from [FEATURE_SPEC.md](FEATURE_SPEC.md). Source code that can be ported
(Apache-2.0) is mapped in [DBEAVER_PORTING.md](DBEAVER_PORTING.md).

## Phase 1: MySQL / MariaDB core ✅ (2026-10-25)

- Workspace, driver traits, SQL lexer and statement splitter (including `DELIMITER`)
- MySQL/MariaDB driver:
  - TLS, compression, init commands, introspection
  - `KILL QUERY` cancel
  - Multiple result sets
- Connection manager with keychain passwords
- Object browser (lazy loading, filter, context menu), query tabs, result grid, messages, status bar
- Safe-update confirmation, automatic `LIMIT`, local query history
- Live tests against MySQL 8.4 and MariaDB 11.4

## Phase 1.5: UI feedback ✅ (2026-10-25)

- Connection tabs in one window; a Connections button and ⌘N to add or edit connections.
- Session restore of connections and query tabs; rename (double-click) and drag-to-reorder
  query tabs.
- Double-clicking a column inserts its backtick-quoted name at the cursor.
- `LIMIT 50` by default, with a "Showing 50 of N rows" footer.
- Inline grid editing (update, set NULL, delete, insert) with transactional save.
- Connection manager:
  - a Connect button on each saved connection
  - top bar with Connect, Test, Save, Cancel and a confirmed Delete
  - password stored in the keychain or an encrypted local file
- Tabs shrink to fit, with an overflow menu. *Open data* writes an explicit `LIMIT 50`.
- The app starts on the connection manager; query tabs restore when you connect.

## Phase 2: Day-to-day essentials

Progress:
- ✅ Export (2026-10-25)
- ✅ Autocomplete (2026-10-25)
- Done along the way:
  - inline tab rename and tab-list dropdowns with a "New" item
  - Save, Discard, + Row and Export in the results bar

- **Grid editing, remaining:**
  - Choose the source table for multi-table results.
  - Duplicate a row.
  - A BLOB/TEXT editor dialog.
  - Warn about unsaved edits when switching results or closing a tab.
- **Table Data tab:** paging (offset and row count), server-side filter and sort.
- **Text and BLOB viewer/editor:** set NULL, load from file, save to file.
- **Export result or table:** CSV, JSON, SQL INSERTs, HTML. Copy as CSV with options.
- **SSH tunnel:** `russh`, password or private key, with secrets in the keychain.
- **Autocomplete:** keywords, functions, databases, tables and columns, through the editor's
  completion provider, with a per-connection cache.
- **History tab:** browse and search `history.toml`, re-run a query.
- **Editor:** open and save `.sql` files, comment/uncomment, change case, format SQL.
- **Object operations:** create, drop, truncate and rename databases and tables; create and alter
  view, procedure, function, trigger and event from templates.
- **Info tab:** server, database and table information.
- **Preferences:** row limit, fonts, theme (light/dark), keybinding preset (F9 vs ⌘↵).

## Phase 3: Power features

- Create/Alter Table designer:
  - Tabs for columns, indexes, foreign keys, checks, advanced options and SQL preview.
- SQL dump backup and restore (execute script) with progress and abort-on-error.
- Import CSV (`LOAD DATA LOCAL INFILE`).
- User manager; SHOW PROCESSLIST, VARIABLES and STATUS views.
- Copy database or tables to another connection.
- Session restore (open tabs, editor contents).

## Phase 4: SQLite

- `ovs-sqlite` driver (`rusqlite`, bundled), file-picker connections, `Dialect::Sqlite`
  (double-quoted identifiers, no `USE`).
- An object browser without the database level (attached schemas instead).

## Phase 5: PostgreSQL

- `ovs-postgres` driver (`tokio-postgres`); schemas between database and objects; sequences,
  types and materialized views.
- `Dialect::Postgres`; cancel through the protocol's cancel request.

## Packaging

- macOS: `.app` bundle and `.dmg` (cargo-packager), code signing and notarization.
- Windows: MSI or NSIS installer, Authenticode signing.
- A GitHub Actions release workflow that attaches the artifacts to tags.
