# Releases

OpenViSQL is a desktop app, so there is no server deployment. Code ships as release binaries.

## How it works today

- CI (`.github/workflows/ci.yml`) runs on every push and pull request to `main`:
  - **Linux:** format check, clippy with `-D warnings`, core/driver/store tests, and live
    driver tests against MySQL 8.4 and MariaDB 11.4 service containers.
  - **macOS and Windows:** builds and tests the whole workspace, including the app.

## Building a release locally

```sh
cargo build --release -p ovs-app
# macOS:   target/release/openvisql
# Windows: target\release\openvisql.exe
```

## Planned

Installers and signing (`.dmg` and MSI) are tracked in [PHASES.md](PHASES.md#packaging).
