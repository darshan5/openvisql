#!/usr/bin/env bash
# Format check, lint and every test, as run before committing. The live database tests run
# against the servers from compose.test.yml when they're up (start them with
# `docker compose -f compose.test.yml up -d --wait`); otherwise those tests skip.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets

up() { nc -z 127.0.0.1 "$1" 2>/dev/null; }
if up 33306 && up 35432 && up 2222; then
    export OVS_TEST_PG=127.0.0.1:35432:postgres:secret OVS_TEST_PG_TLS=1
    export OVS_TEST_SSH=127.0.0.1:2222:tunnel:secret:mysql:3306 OVS_TEST_SSH_PG=postgres:5432
    export OVS_TEST_SSH_KEY="${PWD}/target/test-ssh/id_ed25519"
    export OVS_TEST_COPY_SSH=127.0.0.1:2222:tunnel:secret:mariadb:3306
    # MySQL first, with a copy to MariaDB, then MariaDB on its own.
    OVS_TEST_MYSQL=127.0.0.1:33306:root:secret OVS_TEST_MYSQL_TARGET=127.0.0.1:33307:root:secret \
        cargo test --workspace
    OVS_TEST_MYSQL=127.0.0.1:33307:root:secret cargo test -p ovs-mysql --test live
else
    echo "Test servers aren't running; live database tests will skip." >&2
    cargo test --workspace
fi
