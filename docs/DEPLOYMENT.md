# Releases

OpenViSQL is a desktop app, so there is no server deployment. Releases are installers attached
to GitHub releases.

## Checks

GitHub Actions only builds and publishes releases; there is no CI workflow. Before
committing, run the checks locally:

```sh
docker compose -f compose.test.yml up -d --wait   # once, for the live database tests
scripts/check.sh
```

It checks formatting, runs clippy with `-D warnings`, and runs every test, including the
MySQL, MariaDB, PostgreSQL and SSH suites when the test servers are up. See
[SETUP.md](SETUP.md#live-database-tests) for running suites one at a time.

## Cutting a release

1. Set the version in the workspace `Cargo.toml` (`[workspace.package] version`), commit, and
   push.
2. Tag and push the tag:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. `.github/workflows/release.yml` builds these and attaches them to a **draft** release:
   - `OpenViSQL_<version>_aarch64.dmg` (Apple Silicon)
   - `OpenViSQL_<version>_x64.dmg` (Intel)
   - `OpenViSQL_<version>_x64-setup.exe` (Windows installer, per-user)

   Review the generated notes and publish the release.

It can also be run by hand from the Actions tab (`workflow_dispatch`). The installers are
then kept as workflow artifacts and no release is created.

## Signing

Without secrets, the packages are unsigned. macOS asks users to approve the app in
System Settings → Privacy & Security, and Windows SmartScreen warns about it. Add these
repository secrets to sign:

| Secret | What it is |
| --- | --- |
| `APPLE_CERTIFICATE` | Developer ID Application certificate, exported as `.p12`, base64-encoded |
| `APPLE_CERTIFICATE_PASSWORD` | The `.p12` export password |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | Notarization: Apple ID, app-specific password, team |
| `WINDOWS_CERTIFICATE` | Code-signing certificate as `.pfx`, base64-encoded |
| `WINDOWS_CERTIFICATE_PASSWORD` | The `.pfx` password |

`base64 -i cert.p12 | pbcopy` (macOS) gives the encoded value.

## Packaging locally

Install the packager once, then run it from the app crate:

```sh
cargo install cargo-packager --locked --version 0.11.8
cd crates/ovs-app
cargo packager --release --formats app,dmg   # macOS
cargo packager --release --formats nsis      # Windows
```

Packages are written to `target/packages/`. The packager settings (name, identifier, icons,
minimum macOS version) are in `crates/ovs-app/Cargo.toml` under
`[package.metadata.packager]`. The icon source is `crates/ovs-app/icons/icon.svg`; `icon.icns` (macOS) and `icon.ico` (Windows) are rendered from it.

Building the `.dmg` runs an AppleScript that lays out the Finder window. It needs Finder
automation access. If that times out locally (`AppleEvent timed out`), build just the app
with `--formats app`. GitHub's macOS runners allow it.
