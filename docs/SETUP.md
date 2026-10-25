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

`crates/ovs-mysql/tests/live.rs` runs against real servers when `OVS_TEST_MYSQL` is set:

```sh
docker run -d --name ovs-mysql   -e MYSQL_ROOT_PASSWORD=secret   -p 33306:3306 mysql:8.4
docker run -d --name ovs-mariadb -e MARIADB_ROOT_PASSWORD=secret -p 33307:3306 mariadb:11.4

OVS_TEST_MYSQL=127.0.0.1:33306:root:secret cargo test -p ovs-mysql --test live
OVS_TEST_MYSQL=127.0.0.1:33307:root:secret cargo test -p ovs-mysql --test live
```

Without the variable these tests return early and pass, so `cargo test` works offline.

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
