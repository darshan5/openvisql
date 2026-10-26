# Local Setup

## Prerequisites

- **Rust 1.92 or newer:** `rustup` or Homebrew.
- **macOS:** Xcode Command Line Tools. Full Xcode is not required.
- **Windows:** Visual Studio 2022 Build Tools with the "Desktop development with C++" workload.
- **Docker:** only for the live database tests.
- `git` and the `gh` CLI.

## Getting Started

```sh
git clone git@github.com:darshan5/openvisql.git
cd openvisql
cargo run -p ovs-app          # debug build
cargo run --release -p ovs-app
```

## Lint, format, test

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Live database tests

`compose.test.yml` starts every server the live tests use: MySQL 8.4, MariaDB 11.4,
PostgreSQL 17 (with TLS) and an SSH server that can reach them.

```sh
docker compose -f compose.test.yml up -d --wait
```

Each suite runs when its variable is set. Without the variables these tests return early
and pass, so `cargo test` works offline. The SQLite driver's own tests need no server and
always run.

```sh
# MySQL and MariaDB
OVS_TEST_MYSQL=127.0.0.1:33306:root:secret cargo test -p ovs-mysql --test live
OVS_TEST_MYSQL=127.0.0.1:33307:root:secret cargo test -p ovs-mysql --test live

# PostgreSQL; OVS_TEST_PG_TLS adds the TLS tests
OVS_TEST_PG=127.0.0.1:35432:postgres:secret OVS_TEST_PG_TLS=1 cargo test -p ovs-postgres --test live

# SSH tunnels, signing in with the password and with the generated key, to MySQL and
# PostgreSQL (database addresses as the SSH server sees them); with OVS_TEST_SSH alone, a
# MySQL backup through the tunnel to a local file and a restore back through it
OVS_TEST_SSH=127.0.0.1:2222:tunnel:secret:mysql:3306 OVS_TEST_SSH_PG=postgres:5432 \
  OVS_TEST_SSH_KEY="${PWD}/target/test-ssh/id_ed25519" cargo test -p ovs-ssh -p ovs-app

# Copying a database to a saved connection reached through SSH (here MySQL to MariaDB)
OVS_TEST_MYSQL=127.0.0.1:33306:root:secret \
  OVS_TEST_COPY_SSH=127.0.0.1:2222:tunnel:secret:mariadb:3306 cargo test -p ovs-app copy_dialog

# An existing SQLite file (the tests work on a copy)
OVS_TEST_SQLITE_FILE=/path/to/sample.db cargo test -p ovs-sqlite --test sample
```

The SSH server writes a test key, with passphrase `keypass`, to `target/test-ssh/` on first
start. The database root/postgres password is `secret`, and the SSH user is `tunnel` with
password `secret`.

In zsh, brace variables that come before a `:` (`127.0.0.1:${port}:root`), because `$port:r`
is a history modifier. Otherwise the variable is garbled and the tests skip silently.

Set `OVS_TEST_MYSQL_TARGET` as well to also copy a database across servers (in both
directions, run it twice with the two swapped):

```sh
OVS_TEST_MYSQL=127.0.0.1:33307:root:secret OVS_TEST_MYSQL_TARGET=127.0.0.1:33306:root:secret \
  cargo test -p ovs-mysql --test live copy_database
```

## Where data lives

| What | macOS | Windows |
| --- | --- | --- |
| Connections, history | `~/Library/Application Support/openvisql/` | `%APPDATA%\openvisql\` |
| Passwords | Keychain, service `openvisql` | Credential Manager, `openvisql` |

## Project Structure

```
crates/
  ovs-core/    src/sql/{lexer,splitter,analyze,dialect,edit,keywords}.rs  SQL text utilities
               src/{driver,connection,result,schema,value}.rs             core types and traits
  ovs-mysql/   src/{lib,convert,introspect}.rs, tests/live.rs             MySQL/MariaDB driver
  ovs-postgres/ src/{lib,introspect}.rs, tests/live.rs                   PostgreSQL driver
  ovs-sqlite/  src/lib.rs, tests/sqlite.rs                                SQLite driver
  ovs-store/   src/lib.rs                                                 persistence and secrets
  ovs-app/     src/main.rs        app bootstrap, actions, keybindings, menus
               src/connect.rs     connection manager window
               src/workspace.rs   connection window: toolbar, split, tabs, status bar
               src/browser.rs     object browser tree
               src/query.rs       query tab: editor, execution, results, messages
               src/grid.rs        result grid (DataTable delegate)
               src/runtime.rs     tokio ⇄ GPUI bridge
docs/          architecture, setup, release notes, roadmap, feature spec
```
