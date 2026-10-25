# Feature spec

The target behaviour for OpenViSQL's MySQL/MariaDB client: layout, workflows and defaults. It
lists what to build, not how. Items already shipped are tracked in [PHASES.md](PHASES.md); where
the implementation deliberately differs from this spec, the reason is recorded as an ADR in
[ARCHITECTURE.md](ARCHITECTURE.md).

## 1. Main window

**Frame, top to bottom:**
1. Menu bar.
2. Toolbar:
   - New Connection, New Query Editor, Execute, Execute All, Execute & Edit, Refresh
   - A database combo showing the current default database
   - While a query runs, Execute turns into **Stop**
3. A second toolbar with User Manager, Backup, Execute Script, Copy Database, Export, Manage
   Indexes and Relationships.
4. A row of connection tabs, one per connection. The "+" menu offers "new connection" and "new
   connection with the same settings".
5. Inside a connection:
   - **Left:** the Object Browser, with a filter box above the tree.
   - **Vertical splitter:** defaults to 30% of the width, and its position is remembered.
   - **Right:** the editor tab strip. A Query tab holds the SQL editor on top of the result
     pane, separated by a horizontal splitter whose position is also remembered.
6. Result pane sub-tabs:
   - **N × Result:** one tab per result set.
   - **Messages:** always present.
   - **Table Data, Info, History:** either here or as top-level tabs, depending on a preference.
   - After each run, every tab is renumbered so its title starts with its position
     ("1 Result", "2 Messages"). Positions 1–8 get Alt+N accelerators.
   - Ctrl+Shift+1/2/3 toggle the Object Browser, result pane and editor. Hiding a panel
     collapses its splitter and remembers the previous size.
7. Status bar, seven parts:
   1. General message ("executing…", "batch completed with N errors")
   2. Execution time
   3. Total time
   4. Rows returned
   5. Line and column of the cursor
   6. Connection count
   7. Notifications area

**Menus:**
- **File:** connections, query tabs, open/save SQL, session save/restore, recent files.
- **Edit:** refresh Object Browser; execute submenu; Explain; undo/redo; clipboard; find,
  replace, go to line; hide panels; change case; comment/uncomment.
- **Favorites:** saved snippets.
- **Database:** create, alter, drop, truncate or empty a database; backup; execute script;
  schema as HTML.
- **Table:** paste INSERT, UPDATE, DELETE and SELECT templates; create/alter table; indexes;
  relationships; rename, truncate, drop; duplicate; change engine; import CSV/XML.
- **Others:** per-object create, alter, drop and rename.
- **Tools:** export; backup; flush; diagnostics; History; Info; User Manager; SHOW
  variables, processlist and status; Preferences.

## 2. Connection dialog

**Layout:**
- Along the top: a Saved Connections combo with New, Clone, Save, Rename and Delete.
- Five tabs: MySQL, HTTP, SSH, SSL, Advanced.
- Along the bottom: Connect, Cancel, Test Connection.

**Tabs:**
- **MySQL:**
  - host, user, password, plus a "save password" checkbox
  - port (default 3306)
  - databases, separated by semicolons; blank shows all
  - compressed protocol (default on)
  - session idle timeout, default or custom (28800 s)
  - keep-alive interval
- **SSH:** host, user, port; password or private key, with the key file picker.
- **SSL:** CA, cipher, plus an optional client key and certificate.
- **Advanced:**
  - Object Browser colours for this connection
  - SQL mode: global or custom
  - init commands, separated by semicolons

**Test Connection** connects, validates the database names, and reports the server version.

**Persistence:**
- Connections are saved locally; passwords go to the OS keychain, never to disk in plain text.

## 3. Object Browser

**Tree:**
- user@host
  - Database
    - Tables → each table → Columns, Indexes
    - Views
    - Stored Procs
    - Functions
    - Triggers
    - Events

Folders are gated by server version: views and routines need 5.0.10 or later; events need 5.1.6
or later.

**Loading:**
- Lazy: each unexpanded node has a placeholder child, and its query runs on the first expand.
- F5 refreshes the tree and restores the expanded and selected nodes.

**Queries per level:**

| Level | Query |
| --- | --- |
| Databases | SHOW DATABASES, or the configured database list |
| Tables | SHOW FULL TABLES, keeping base tables only |
| Views, procedures, functions, events | information_schema |
| Triggers | SHOW TRIGGERS |
| Columns | DESCRIBE; nodes show the name, type and "Nullable" |
| Indexes | SHOW INDEX; nodes show the name and column list |

**Filter:**
- Applies 200 ms after the last keystroke, as a case-insensitive substring match.
- Its scope depends on the selection: databases at the root, tables inside a database.
- Esc clears it; Ctrl+Shift+B focuses it.

**Context menus** reuse the main menus for each node type.

**Keyboard and mouse:**
- Delete drops the object, Shift+Delete truncates it, F2 renames, F4 creates, F6 alters.
- Double-click opens the object.
- Dragging an object into the editor inserts its name.
- Generated templates:
  - INSERT: all columns
  - UPDATE: all columns, with a WHERE on the primary key
  - DELETE: WHERE on the primary key
  - SELECT: column list plus a LIMIT

## 4. Query execution

**Execution modes:**
- **F9, Execute Current:** runs the selection if there is one; otherwise the statement under
  the cursor. When the cursor sits right after a statement, that statement runs.
- **Ctrl+F9 / Shift+F9, Execute All:** runs every statement in order. It halts on the first
  error only when a preference says so (default off).
- **F8, Execute & Edit:** runs, then opens the grid in edit mode.
- **Explain / Explain Extended:** prefixes the statement before running it.

**Splitting:**
- The `DELIMITER` command changes the delimiter for the rest of the script.
- The delimiter is ignored inside quotes, backticks and comments.
- `-- ` comments need a space after the dashes.
- Line and offset bookkeeping lets the cursor be matched to a statement.

**Automatic LIMIT:**
- A top-level SELECT with a FROM and no LIMIT gets `LIMIT 0, <page>` appended.
- The page size defaults to **50**.

**Stop:** interrupts the running statement with `KILL QUERY` from a second connection, which
keeps the session's state (variables, temporary tables, default database).

**Results and Messages:**
- Every result set becomes its own Result tab.
- The Messages tab lists, for each statement: the statement text, rows returned or affected,
  warnings or the error, and execution, transfer and total time.
- A header summarises the run, and an optional setting appends SHOW WARNINGS output.
- Focus moves to Messages if there were errors, otherwise to the first result.

**History:** every statement sent is logged with a timestamp, elapsed time and any error. The
log is read-only and capped at 50,000 lines.

## 5. Result grid

**Toolbar:** export, copy all, a table combo, insert, duplicate, save, delete, cancel, and
grid / form / text views.

**Editing:**
- The table combo always starts with "(Read Only)", followed by every source table found in
  the result's metadata.
- Choosing a table makes only that table's columns editable.
- **UPDATE:**
  - SET lists only the columns that changed.
  - WHERE uses the primary key.
  - Without a usable primary key, WHERE compares every column (with IS NULL for nulls) and adds
    `LIMIT 1`. If more than one row matches, the user is warned first.
- **INSERT:** lists only the columns that were set.
- **After saving:** the row is re-fetched.

**Display:**
- NULL is shown as `(NULL)`.
- A BLOB/TEXT viewer opens with F11. It has text, image and JSON views, load/save to file, and
  set NULL.

**Paging:** a limit checkbox, an offset, a row count, and previous/next buttons.

**Sorting:**
- Result tabs sort on the client side.
- Table Data tabs filter and sort on the server, re-querying with WHERE and ORDER BY.

**Export formats:**
- CSV, with terminators, enclosure, escape, NULL text and a header option
- JSON, HTML, XML, Excel XML
- SQL: structure, data, or both

## 6. Table Data, Info, table designer

- **Table Data:** select-all on the selected table, plus filter, sort and limit (50 rows by
  default). It is editable and has a foreign-key value lookup.
- **Info:**
  - Server: version, host, user, port, charset, uptime.
  - Database: CREATE DATABASE and the table-status list.
  - Table: fields, keys and CREATE TABLE.
  - Routines: SHOW CREATE output.
- **Create/Alter Table designer:**
  - Header row: name, database, engine, charset, collation.
  - Sub-tabs:
    1. **Columns:** name, type, length, default, PK, binary, not null, unsigned,
       auto-increment, zerofill, charset, collation, on update, comment, generated/virtual,
       check.
    2. Indexes
    3. Foreign keys
    4. Check constraints
    5. Advanced table options
    6. SQL Preview
  - Closing with unsaved changes prompts to save.

## 7. Highlighting and autocomplete

- **Highlighting:** keywords and functions come from a bundled list, filtered by minimum
  server version. Upper/lower-case conversion while typing is optional.
- **Autocomplete:** keywords, functions, databases, tables and columns, from a per-connection
  cache of database objects with a "rebuild" command.

## 8. Keyboard shortcuts (Windows)

| Area | Bindings |
| --- | --- |
| Execute | F9 current; Ctrl+F9 or Shift+F9 all; F8 execute and edit |
| Object Browser | F5 refresh; Ctrl+B focus tree; Ctrl+Shift+B focus filter |
| Tabs | Ctrl+T new query; Ctrl+W close; Ctrl+PgUp/PgDn switch tabs |
| Connections | Ctrl+M new connection; Ctrl+N clone the current connection |
| Results | Alt+1…8 select result tab; Ctrl+R focus results; Ctrl+L grid/text |
| Objects | F4 create; F6 alter; F2 rename; Del drop; Shift+Del truncate; F7 manage indexes |
| Templates | Alt+Shift+I / U / D / S paste INSERT / UPDATE / DELETE / SELECT |
| Other | Ctrl+Shift+H History; Ctrl+Shift+I Info; Ctrl+U User Manager |

## 9. Import and export

- **SQL dump backup:**
  - Choose objects; structure, data, or both.
  - Source options: lock tables, flush logs, single transaction.
  - Options written into the file: DROP statements, CREATE DATABASE, bulk inserts, foreign-key
    checks off, BLOBs as hex.
- **Execute SQL script** through the same splitter, with abort on error.
- **Import CSV** (`LOAD DATA LOCAL INFILE`) and **import XML**.
- **Copy database or tables** to another connection.
- **Schema documentation** as HTML.
