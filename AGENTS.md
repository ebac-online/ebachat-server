# ebachat-server

A chat server in Rust that replaces the Mattermost server (Go) for EBAC's internal chat. Its client is EBAChat
([ebac-messenger](https://gitlab.ebac.dev/ebac-online/ebac-messenger)), a native Mattermost client.

## Goal

- **Same database.** For now the server runs on an existing Mattermost PostgreSQL database without changing its
  schema. The Go server and this one must be able to take turns on the same database: whatever one writes, the
  other reads correctly. Until we drop compatibility, never add, alter or drop tables or columns, and never write
  rows in a shape the Go server would not write. The one exception is what the Go server cannot see: extra indexes
  (and the like) as our own migrations in `migrations/ebachat/` (see `migrations/README.md`).
- **Same protocol.** REST API v4 and the websocket events, as the Mattermost server sends them: paths, status codes,
  JSON field names, empty values (`""` vs `null` vs missing), ordering and pagination. EBAChat and the stock
  Mattermost web and mobile clients must work against it unchanged.
- **Lighter.** Less CPU and memory than the Go server at the same load. That is the reason for the rewrite.

## Reference

- The reference is the Mattermost server's behaviour, not guesses. Source: Mattermost v12.1.0, checked out (not
  tracked) at `../ebac-messenger/mattermost/`. Schema: `server/channels/db/migrations/postgres/`. Stores (the SQL
  that reads and writes each table): `server/channels/store/sqlstore/`. Handlers: `server/channels/api4/`. API
  structs and JSON: `server/public/model/`.
- The target is Mattermost 12.1.0: its schema and its API. The schema's migrations are copied verbatim into
  `migrations/postgres/` (see `migrations/README.md`). Production `mm.ebac.app` still runs Team Edition 11.11.2,
  so its database has to reach the 12.1.0 schema before this server can run on it.
- Where the code is unclear, check against a live server: `docker run -d --name mm-preview -p 8065:8065
  mattermost/mattermost-preview`, seeded with `../ebac-messenger/scripts/seed-local.ps1` and `seed-local-more.ps1`
  (accounts `admin` / `Admin-pass-123`, `alice` / `Alice-pass-123`). Compare responses byte for byte.
- `mm.ebac.app` is read-only for us: public endpoints only, no logins, no writes, and never connect to its database.

## Code

- `cargo fmt --all` before a commit; CI checks it.

## License

- This project is AGPL-3.0-only (see `LICENSE`, `NOTICE`), because it is based on the Mattermost server's
  AGPL-3.0 source.
- **Never read, port or copy anything from `server/enterprise/`.** It is under the Mattermost Source Available
  License, not AGPL. Enterprise features are written from documentation and observed behaviour only.
  `server/einterfaces/` (AGPL) only declares their interfaces.
- `server/public/`, `server/i18n/`, `server/templates/` and `webapp/` are Apache-2.0: they can be reused; keep their
  copyright notice and mark the file as changed.
- A file ported closely from Mattermost code says so in its header: `// Based on Mattermost <path>, Copyright (c)
  2015-present Mattermost, Inc. Licensed under AGPL-3.0.`
- Dependencies must be AGPL-3.0-compatible: MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0 are fine; SSPL, BUSL,
  Commons Clause and proprietary licenses are not.
- "Mattermost" is a trademark of Mattermost, Inc.: never in the project's name, logo or UI. "Compatible with
  Mattermost" in descriptions is fine.
