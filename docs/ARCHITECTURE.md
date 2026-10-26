# Architecture

## Overview

```
┌──────────────────────── ovs-app (GPUI Kit) ─────────────────────────┐
│ ConnectView ─► Workspace (one window per connection)                │
│                 ├─ ObjectBrowser  ── metadata session               │
│                 ├─ QueryTab × N   ── one session per tab            │
│                 │    ├─ Editor (tree-sitter SQL)                    │
│                 │    └─ ResultGrid (DataTable) + Messages           │
│                 └─ StatusBar                                        │
│ runtime.rs: GPUI tasks ⇄ tokio runtime (driver I/O)                 │
└───────────────┬───────────────────────────────┬─────────────────────┘
                │ Driver / Session traits       │
        ┌───────▼────────┐              ┌───────▼────────┐
        │   ovs-core     │◄─────────────│   ovs-mysql    │
        │ types, SQL     │              │ mysql_async    │
        └───────▲────────┘              └────────────────┘
                │
        ┌───────┴────────┐
        │   ovs-store    │  connections.toml, history.toml, OS keychain
        └────────────────┘
```

- **`ovs-core`** knows nothing about any engine or UI. A new engine implements `Driver` and
  `Session` and adds a `Dialect` variant.
- **Sessions are not shared between concurrent users.** The object browser has its own session,
  and every query tab opens another, so a long query never blocks browsing.
- **Async bridging:** GPUI runs its own executor, while the drivers need tokio. `runtime::run`
  spawns driver futures on a shared two-thread tokio runtime and awaits the `JoinHandle` from
  GPUI tasks. Entity updates always happen back on the GPUI foreground.
- **Cancellation:** the MySQL protocol can't interrupt a statement in-band, so `CancelHandle`
  opens a short-lived second connection and issues `KILL QUERY <id>`. Killing only the query,
  not the connection, keeps session state such as variables and temporary tables.

## Licensing and third-party code

OpenViSQL is MIT-licensed, so its code must stay compatible with MIT:

- **Copyleft code is never copied or translated.** That covers GPL, LGPL and EPL code,
  including line-by-line ports to Rust. Keyword lists, queries and algorithms are written from
  public MySQL and MariaDB documentation.
- **Permissively licensed code may be ported.** DBeaver Community (Apache-2.0) code and data
  may be ported, with the attribution described in
  [DBEAVER_PORTING.md](DBEAVER_PORTING.md#rules-for-ported-code). The exceptions are its
  EPL-licensed files and bundled third-party code.
- **Target behaviour** is described in our own words in [FEATURE_SPEC.md](FEATURE_SPEC.md).

---

## ADR-001: All-Rust GUI with GPUI Kit

- **Date:** 2026-10-25
- **Context:** The goal is native clients for macOS and Windows from a Rust rewrite. Three
  options were considered:
  1. A Rust core with separate SwiftUI and WinUI shells.
  2. An all-Rust GUI.
  3. A Rust core with a Tauri web UI.
- **Decision:** An all-Rust GUI on GPUI Kit (`gpui-kit` 0.8, Apache-2.0). It is GPU-rendered
  and ships a virtualized `DataTable`, a tree-sitter code `Editor`, `Tree`, resizable panes,
  dialogs and a status bar, which are exactly the pieces a SQL client needs.
- **Consequences:**
  - One codebase and one language.
  - Controls are custom-drawn, not OS widgets.
  - GPUI's API is pre-1.0 and changes between releases, so the `gpui-kit` version is pinned
    and upgraded deliberately.
  - Zed's own editor crate is GPL and must not be used; GPUI Kit's editor is Apache-2.0.

## ADR-002: Text protocol, values as text where precision matters

- **Date:** 2026-10-25
- **Decision:** Queries use the text protocol (no prepared statements). DECIMAL, DATE, TIME
  and DATETIME values travel as the server's own text. Integers and floats are parsed for
  sorting and alignment.
- **Consequences:**
  - The grid shows exactly what the server returned.
  - Edits round-trip without precision loss.
  - No server-side statement cache builds up for each tab.

## ADR-003: Passwords in the keychain or an encrypted local file

- **Date:** 2026-10-25 (revised the same day to add the local option)
- **Context:**
  - Encrypting passwords in a settings file with a key that ships in the binary is
    obfuscation, not protection.
  - The OS keychain can prompt for access, and does so again after every rebuild of an
    unsigned development binary.
- **Decision:** each connection chooses where its password goes (`PasswordStore`):
  - **Keychain:** the platform credential store through the `keyring` crate, keyed by
    connection UUID.
  - **Local:** XChaCha20-Poly1305 in `secrets.toml`, with each password bound to its
    connection id as associated data. The key is random, created on first use, and kept in
    `secret.key` (mode 0600).
  - `connections.toml` never contains secrets.
  - Passwords are read only when connecting and cached for the process. Unchanged passwords
    are never rewritten, so a connection that doesn't use the keychain never touches it.
- **Consequences:** the local store protects against the settings files being copied or read
  by other accounts. It does not protect against someone who can already read the user's own
  files, because the key sits beside them.

## ADR-004: Automatic `LIMIT` for plain SELECTs

- **Date:** 2026-10-25 (revised the same day: 1000 → 50 at the user's request)
- **Decision:**
  - A top-level `SELECT` with no `LIMIT`, `INTO`, `FOR UPDATE` or `LOCK` clause gets
    `LIMIT 50` appended.
  - When the limit cuts the result short, a `SELECT COUNT(*) FROM (<query>)` runs afterwards
    so the footer can say "Showing 50 of N rows". If that count fails (for example, duplicate
    column names in a join), the footer says more rows may exist.
  - Other statements that return rows (`SHOW`, `CALL`) are capped at 50,000 rows client-side
    and marked "(limited)".

## ADR-005: Grid editing writes through keys and a transaction

- **Date:** 2026-10-25
- **Decision:**
  - Only results of a plain SELECT whose columns come from one table are editable;
    expression columns stay read-only.
  - Rows are located by the primary key, then by a unique index whose columns are all
    NOT NULL. A key is used only if all of its columns are present in the result. This matches
    DBeaver's `getBestIdentifier`; see DBEAVER_PORTING.md.
  - Without a usable key, rows are matched on every non-float column with `LIMIT 1`, so at
    most one row changes.
  - Saving runs every statement in one transaction, rolls back on the first error, and
    re-runs the query afterwards.
- **Consequences:** MyISAM tables can't roll back. The footer warns when the all-columns
  fallback is in use.

## ADR-006: Query tabs persist per connection; nothing connects at startup

- **Date:** 2026-10-25 (revised: auto-reconnect removed at the user's request)
- **Decision:**
  - `session.toml` records each connection's query tabs (title, database, SQL), keyed by
    connection id. It is written 700 ms after the last change, on close, and on quit.
  - The app always opens on the connection manager with no open connections.
  - Connecting to a connection restores its saved tabs.
- **Consequences:**
  - Startup never touches the network or the keychain.
  - Saved tabs for a deleted connection are simply never restored.

## ADR-007: SSH tunnels shared per connection, trust-on-first-use host keys

- **Date:** 2026-10-26
- **Decision:**
  - `ovs-ssh` (on `russh` with the `ring` backend, so no CMake or NASM is needed on Windows)
    forwards a local port to the database through an SSH server.
  - The app keeps one tunnel per connection id in `runtime.rs`. The object browser and every
    query tab connect to `127.0.0.1:<local port>`, and a dropped tunnel is reopened on the
    next connect.
  - The SSH password or key passphrase is stored like the database password (keychain or
    encrypted file, under `<id>:ssh`).
  - Host keys are recorded in `known_hosts.toml` on first connection. A different key is
    refused, as OpenSSH does.
- **Consequences:**
  - With SSL "verify certificate", the server name checked is the tunnel's local end, so
    "encrypt only" is the practical SSL mode through a tunnel.
  - Live tests run against an Alpine sshd container.

