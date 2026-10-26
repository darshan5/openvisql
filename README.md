# OpenViSQL

A fast, native desktop SQL client written in Rust, for macOS and Windows.

OpenViSQL connects to **MySQL** and **MariaDB** today; **SQLite** and **PostgreSQL** are next.
The layout is the classic one for MySQL tools: an object browser on the left, query tabs on
the right, and results below.

![OpenViSQL: object browser, query tabs, SQL editor and result grid](docs/images/screenshot.png)

## Features

- Several connections open at once as tabs across the top, with tabs that shrink to fit. The
  app starts on the connection manager with nothing connected. Each connection's query tabs
  (titles, SQL, database) come back when you connect to it again.
- Saved passwords are never in plain text. Each connection chooses where its password goes:
  - the OS keychain (macOS Keychain or Windows Credential Manager), or
  - an encrypted file on this computer, which never shows a system prompt.
- TLS (encrypt-only or full certificate verification), compressed protocol, init commands.
- Object browser with lazy loading: databases → tables, views, procedures, functions,
  triggers, events → columns and indexes. Includes a filter box and context menus (open data,
  show `CREATE`, copy name).
- SQL editor with syntax highlighting, line numbers, find and replace.
- Execute the statement under the cursor or the selection (**F9** / **⌘↵**), or everything
  (**Ctrl+F9** / **⇧⌘↵**). Handles `DELIMITER` for stored-program bodies.
- Runs each query tab on its own connection, so long queries never block the rest of the app.
  **Stop** (**⌘.** / **Ctrl+.**) cancels with `KILL QUERY`.
- *Open data* writes `SELECT … LIMIT 50` into the editor, and SELECTs without a limit get
  `LIMIT 50` automatically. The results footer shows "Showing 50 of 20,000 rows" whenever a
  limit cut the result short. Multiple result sets open in separate tabs, and a Messages tab shows rows affected,
  warnings, errors and timings.
- Query tabs can be renamed with a double click and reordered by dragging.
- Double-clicking a column in the object browser inserts `` `column` `` at the cursor.
- **Inline editing** of single-table SELECT results:
  - Double-click a cell to edit it. Right-click a row to set a cell to NULL, delete the row,
    or add a row.
  - **Save** applies the changes in one transaction, then re-fetches the rows.
  - Rows are matched by primary key, or a NOT NULL unique key; without either, by all columns
    plus `LIMIT 1`.
- Virtualized result grid that stays fast with tens of thousands of rows. Sort by column, and
  copy rows as TSV.
- Asks for confirmation before running `UPDATE`/`DELETE` without a `WHERE` clause.
- Query history is saved locally.

## Install and run

Prerequisites:
- Rust 1.92 or newer
- On Windows: Visual Studio Build Tools

```sh
git clone https://github.com/darshan5/openvisql.git
cd openvisql
cargo run --release -p ovs-app
```

See [docs/SETUP.md](docs/SETUP.md) for development setup and running the integration tests.

## Workspace layout

| Crate | What it does |
| --- | --- |
| `ovs-core` | Engine-agnostic types, driver traits, SQL lexer/splitter, quoting, grid-edit SQL |
| `ovs-mysql` | MySQL/MariaDB driver built on `mysql_async` |
| `ovs-store` | Saved connections, passwords (keychain or encrypted file), history, session |
| `ovs-app` | The desktop app, built with [GPUI Kit](https://gpui-kit.com) |

## License

[MIT](LICENSE)
