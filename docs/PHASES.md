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
- ✅ Result paging, WHERE filter and server-side sorting (2026-10-26)
- ✗ History tab: dropped at the user's request. Statements are still saved to
  `history.toml`.
- ✅ Editor basics: open/save `.sql`, toggle comment, change case (2026-10-26)
- ✅ Grid editing extras (2026-10-26):
  - unsaved-edit warnings
  - TEXT/BLOB value editor
  - duplicate row
  - choose the table to edit for joins
- ✅ Object operations and Info (2026-10-26):
  - create/drop/truncate/rename
  - CREATE templates
  - statement templates
  - Info tabs
- ✅ SSH tunnels (2026-10-26)
- ✅ Preferences (2026-10-26), in Settings (⌘, / Ctrl+,):
  - row limit / page size
  - editor font size
  - theme: system, light or dark
  - stop "Execute all" at the first error

Phase 2 is complete.
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
  - ✅ Columns (name, type, NULL, primary key, auto-increment, default, comment, order) with a
    live `CREATE TABLE` / `ALTER TABLE` preview; opened from the object browser.
  - ✅ Indexes (INDEX, UNIQUE, FULLTEXT, SPATIAL, prefix lengths); renamed columns carry
    their indexes along.
  - ✅ Foreign keys (multi-column, cross-database, ON DELETE / ON UPDATE) and table options
    (engine, collation, comment).
  - ✅ CHECK constraints (re-created when a column they use is renamed).
  - ✅ Sections in tabs (columns, indexes, foreign keys, checks) with the SQL preview below.
  - ✅ Partitions (MySQL and MariaDB): the table's `PARTITION BY` clause, edited as SQL in
    its own tab. A change repartitions with `ALTER TABLE … PARTITION BY`; clearing it runs
    `REMOVE PARTITIONING`. Partitioned tables can't have foreign keys, which is checked first.
- ✅ SQL dump backup and restore (run script) with abort-on-error.
- ✅ Import CSV (batched `INSERT`s in one transaction, rolled back on error).
- ✅ Process list, variables and status views.
- ✅ User manager: create, rename, drop, change password; global and per-database privileges
  with GRANT OPTION.
- ✅ Copy a database or table to another connection of the same engine (or another database
  on the same one), adapting collations between MySQL and MariaDB. The target can be open in
  a tab or only saved: a saved one is connected for the copy, directly or through its SSH
  tunnel, and the dialog asks for its password if none is saved.
- ✅ Session restore (open tabs, editor contents).

## Phase 4: SQLite

- ✅ `ovs-sqlite` driver (`rusqlite`, bundled), with column origins for grid editing, cancel
  through the interrupt handle, and introspection from `sqlite_master` and pragmas.
- ✅ File connections: open or create a file from the connection manager; no password or
  keychain involved.
- ✅ `Dialect::Sqlite` (double-quoted identifiers, `BEGIN`, rowid-targeted keyless edits) used
  by templates, paging, grid edits, export and CSV import.
- ✅ Object browser shows `main` and attached databases with tables, views and triggers;
  server-only menus are hidden.
- ✅ Backup to a SQL script, restore by running it, and copy tables between SQLite files.
- ✅ Table designer for SQLite: changes rebuild the table in one transaction (new table, copy
  rows, swap), keeping its indexes, triggers and `STRICT` / `WITHOUT ROWID`; a lone rename
  uses `ALTER TABLE … RENAME TO`.

Phase 4 is complete.

## Phase 5: PostgreSQL

- ✅ `ovs-postgres` driver (`tokio-postgres`). Statements are prepared to learn column types
  and source tables, then run with the simple query protocol so every type displays as
  PostgreSQL formats it. Cancel uses the protocol's cancel request.
- ✅ `Dialect::Postgres`: dollar-quoted bodies and standard strings in the splitter, `'\x..'`
  bytea literals, ctid-targeted keyless edits, `LIMIT n OFFSET m` paging.
- ✅ The connected database's schemas are the object browser's databases, with tables,
  views, functions, procedures and triggers. `CREATE` statements for tables are built from
  the catalogs.
- ✅ Templates, info queries and server tools (`pg_stat_activity` with cancel/terminate,
  settings, statistics).
- ✅ TLS: "encrypt only" or full verification against the web roots plus an optional CA
  file. Cancel requests are encrypted too.
- ✅ Backup, restore and copy of a schema. Dumps leave out the schema name so they restore
  anywhere. Foreign keys are added after the data. Identity and serial counters continue
  after the restored ids.
- ✅ Table designer for PostgreSQL. A change runs in one transaction: renames, type /
  nullability / default / identity changes, keys, indexes, checks, comments and the table's
  name. Column reordering is refused, since PostgreSQL keeps column order.
- ✅ Sequences and types (enums, domains, composite and range types) in the object browser,
  with their `CREATE` statements. Materialized views can be refreshed. Drops say what
  PostgreSQL needs: `DROP MATERIALIZED VIEW`, `DROP DOMAIN`, a function's arguments.
- ✅ Dumps create types and free-standing sequences before the tables that use them, only when
  missing, and carry sequence values over. Replacing a whole schema drops its tables with
  `CASCADE` so views don't block it.

Phase 5 is complete.

## Phase 6: Visual tools

- ✅ Visual query builder (right-click a database or table → "Query builder…"), in a window
  of its own. Tables from a searchable list become boxes on a canvas: tick columns (or `*`)
  to select them, and drag from a column to another table's column to join on it; foreign
  keys are joined automatically, and new boxes are placed beside the table they join. Each
  join line has a marker to change its kind (INNER, LEFT, RIGHT, FULL where supported) or
  remove it. A grid sets each column's output, alias, sort type and order, aggregate,
  grouping and criteria (`> 100`, `LIKE 'a%'`, `IS NULL`, or a value). Distinct, limit and
  offset; further queries combined with UNION, UNION ALL, INTERSECT or EXCEPT in tabs. Live
  SQL; "Open in editor" or "Run" in the connection's window. The generated SQL is tested on
  MySQL, MariaDB, PostgreSQL and SQLite.
- ✅ ER diagram (right-click a database → "ER diagram…"), in a window of its own: tables as
  boxes listing their columns (primary keys `#`, foreign keys `→`), lines between key
  columns. Laid out automatically: referenced tables left of the tables referring to them,
  each column ordered to avoid crossings and boxes level with what they connect to;
  unrelated tables below. Pan, zoom, drag tables, rearrange, and export as SVG.

## Packaging

- ✅ macOS `.app` bundle and `.dmg`, Windows NSIS installer (cargo-packager), with an app icon.
- ✅ A GitHub Actions release workflow that builds them on tags (Apple Silicon Macs and
  Windows) and attaches them to a draft release. Intel Macs aren't built (decided
  2026-10-26); they can build from source.
- ✅ Signing and notarization run when the certificate secrets are set; see
  [DEPLOYMENT.md](DEPLOYMENT.md#signing). Releases are unsigned for now (decided 2026-10-26).
- Auto-update: not planned (decided 2026-10-26). New versions are downloaded from the
  releases page.

