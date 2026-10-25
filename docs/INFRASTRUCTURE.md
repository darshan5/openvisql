# Infrastructure

## GitHub Repository

- **URL:** https://github.com/darshan5/openvisql
- **Visibility:** public
- **Default Branch:** main
- **CI:** GitHub Actions, `.github/workflows/ci.yml` (Linux, macOS, Windows)
- **Created:** 2026-10-25

## Hosting

None. OpenViSQL is a desktop application, so it has no subdomain, DNS record or Dokploy
deployment.

## Local services for development

| Service | Image | Port | Credentials |
| --- | --- | --- | --- |
| MySQL | `mysql:8.4` | 33306 | root / secret |
| MariaDB | `mariadb:11.4` | 33307 | root / secret |

These are disposable test containers; see [SETUP.md](SETUP.md#live-database-tests).
