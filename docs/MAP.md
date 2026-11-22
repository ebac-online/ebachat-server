# Map of the Go server

What the Mattermost 12.1.0 server does, what of it is actually used, and in what order we port it.

## Files

| File | What | How |
| --- | --- | --- |
| `docs/map/routes.tsv` | Every REST route of `channels/api4`, one row per method | `make map` (`tools/mmmap`) |
| `docs/map/ws-events.tsv` | Every websocket event of the model, what emits it, whether EBAChat handles it | `make map` |
| `docs/prod-traffic.tsv` | Requests to `mm.ebac.app` by method and normalised path, by client | nginx-proxy log on the VM, see below |

Columns of `routes.tsv`:

- `tier`
- `auth`: the handler wrapper. `APISessionRequired`, `APIHandler` (no session), `...TrustRequester` (no CSRF check).
- `handler` and `source`, `file:line` under `server/`.
- `prod_count`: requests in the 4 h sample.
- `ebachat`: call sites in `ebac-messenger/src`.
- `permissions`, `ws_events`, `store_calls` and `tables`: everything reachable from the handler through `app` and
  `sqlstore`.
- `go_loc_reachable`: lines of Go behind the route.
- `rust`: empty, to be filled in as routes are ported.

The call graph comes from `go/ast` without type checking: calls are resolved by receiver type and method name. It
over-approximates, so read `tables`, `ws_events` and `go_loc_reachable` as upper bounds.

## Tiers

| Tier | Rule | Routes | Share of production requests |
| --- | --- | ---: | ---: |
| T1 | EBAChat calls it | 123 | 61% |
| T2 | Not EBAChat, but production traffic (web, desktop, mobile, bots) | 37 | 12% |
| stub | Production traffic, but an area we do not port (license, plugins, AI). Answer as Team Edition without a license does | 11 | 16% |
| T3 | Nobody calls it, area kept | 228 | 0 |
| skip | Nobody calls it, area not ported (cluster, LDAP, SAML, cloud, compliance, ...) | 219 | 0 |

There are 607 routes in all. Another 172 local-mode routes (`APILocal`, for `mmctl --local`) are dropped. The remaining
11% of production requests are plugins (`/plugins/...`, mostly Calls) and static files.

- **T1 is the first milestone.** EBAChat works against the Rust server.
- **T1 + T2 + stub is the second.** Stock web, desktop and mobile clients work.
- **T3 is ported on demand.**

By area: routes with T1/T2/stub, and production requests in 4 h:

| Area | Routes | T1 | T2 | stub | Requests |
| --- | ---: | ---: | ---: | ---: | ---: |
| channels | 71 | 30 | 4 | | 84,470 |
| users | 79 | 22 | 8 | | 72,258 |
| properties | 13 | | | 4 | 31,256 |
| posts | 31 | 15 | 5 | | 22,072 |
| threads | 7 | 6 | 1 | | 19,163 |
| plugins | 12 | | | 1 | 12,436 |
| teams | 55 | 18 | 4 | | 11,823 |
| files | 10 | 6 | 2 | | 8,716 |
| agents | 2 | | | 2 | 7,701 |
| notifications | 2 | 1 | 1 | | 7,033 |
| drafts | 4 | 4 | | | 6,464 |
| config, license, preferences, emoji, status, roles, reactions, system, hooks, commands, ... | | 17 | 12 | 4 | 9,800 |

**The T1 routes reach about 1,400 Go functions, about 42k lines (upper bound), over 51 tables.** That is the volume
behind the first milestone, against about 220k lines in `api4` + `app` + `sqlstore` + `model`.

## Websocket

EBAChat handles 31 of the 108 events. Most of the events it handles come from routes in T1, for example `posted`
(42 routes), `thread_updated` (45), `user_updated` (38), `channel_member_updated` (22) and `ephemeral_message` (46).
Three events are not emitted by any route:

- `hello`, from the hub on connect;
- `status_change`, from the status service;
- `draft_updated`.

In the 4 h sample there were 1,121 websocket connections, all 101. About 330 client connections are open at any time.
99% of reconnects resume with `connection_id` and `sequence_number`, so the resume buffer (the hub's
missed-events queue) is T1, not an extra.

## Production: `mm.ebac.app`

Source:

- **Host:** VM `prod-ebac-mattermost` (8 vCPU, 24 GB), docker compose in `/home/ubuntu/mm`.
- **Services:** Mattermost Team Edition 11.11.2, Postgres 16, `nginx-proxy`.
- **Not in k8s:** it has no Kibana index and no ingress metrics.
- **Requests:** the nginx-proxy access log, 2026-11-19 16:59–21:07 UTC (4 h of weekday business hours). It was
  recreated that day, and rotation keeps only about 30 h anyway.
- **No latency:** the log format has no `request_time`.

Load:

- **Requests:** 22 req/s on average, 35 req/s in the peak minute, 403 client IPs.
- **Data:** 1,200 users, 526 sessions, 10.0M posts, 13.4 GB database.

Clients:

| Client | Requests |
| --- | ---: |
| Desktop | 59% |
| Web | 21% |
| Mobile | 10% |
| Bots | 6% |
| `ureq` (EBAChat, presumably) | 4% |

Resources, the baseline to beat:

| Process | CPU | Memory |
| --- | --- | --- |
| Mattermost | 0.045 cores on average | 825 MB anon, cgroup peak 2.3 GB |
| Postgres | 0.14 cores on average, 65% of a core at a snapshot | 7.4 GB cgroup, mostly cache |

**Postgres spends about 3× Mattermost's CPU.** Sequential scans read about 17 billion `Threads` rows and 5.6 billion
`PostsPriority` rows a day, likely the threads endpoints (`totalsOnly`, `unread`). Same schema, but different queries
are allowed. This is probably the biggest win of the rewrite, and it lies in SQL, not in the language.

Hot endpoints (4 h):

| Request | Count | Note |
| --- | ---: | --- |
| `POST /users/status/ids` | 34.5k | |
| `POST /channels/members/me/view` | 26.6k | |
| `GET /channels/{id}/posts` | 19.8k | 96% `since` |
| `POST /posts` | 15.7k | 79% bots |
| `POST /users/ids` | 13.5k | `since` |
| Client bootstrap | about 10.3k each | categories, unread, channels, channel_members, properties, plugins |
| `GET /users/{id}/teams/{id}/threads` | 9.6k | |

The bootstrap runs about 25× per device in 4 h: desktop re-syncs often.

## Decisions

- **Plugins: not now.** Production runs Calls (32k polls in 4 h), Jira (webhook in, settings), gcal and
  mattermost-ai. Go plugins cannot run in a Rust server, and none of them is ported for now. `/plugins/*` and
  `GET /api/v4/plugins/webapp` answer as a server with no plugins.
- **Web client: served as Go serves it** (`src/web.rs`, from `channels/web/static.go`), so the stock web and
  desktop clients load from the server.
  - **What is served:** `/static/*` from the client directory, with Range and conditional requests, and
    compressed as Go's `gzhttp` does; `/static/plugins/*` from `PluginSettings.ClientDirectory`;
    `/robots.txt`. Every other GET or HEAD outside `/api/` gets `root.html`, with the site name and description.
    It is byte-identical to Go's. Old browsers and desktop apps get Go's unsupported pages instead. Details in
    `docs/WEB.md`.
  - **The client directory:** `EBACHAT_CLIENT_DIR`, default `client/`. Use the released web client of the same
    version (Apache-2.0); `dev/get-webapp.sh` fetches 12.1.0 into `dev/webapp`, linked as `dev/client`. Go finds
    the same `dev/client` next to its binary's directory.
  - **EBAChat needs only `/static/emoji/{code}.png`,** the system emoji images. Without a full client, point
    `EBACHAT_CLIENT_DIR` at `webapp/channels/src/images`, which holds `emoji/`.
  - **Checked with Mattermost Desktop 6.2.2** on the Rust server: as a user, login, channels, posting, threads,
    reactions, search, DMs, file paste with thumbnail and preview, and the theme. As admin, System Console pages.
- **Web routes outside `/api/`** (`src/web.rs`): their errors are Go's web error page, an HTML page or a 3xx
  redirect to the client's signed `/error?message=...&s=...` (`RenderWebAppError`); JSON only with `X-Mobile-App`.
  - **Served:** `GET|HEAD /files/{id}/public`, `GET|POST /login/sso/saml` (always "SAML not available", 302),
    `/manualtest` (the client's page unless `EnableTesting`). Also the OAuth 2.0 provider, GitLab SSO sign-in,
    `/oauth/intune` and `/login/one_time_link`: see `docs/WEB.md`. A path no route takes gets the client's page
    for GET and HEAD, and Go's plain-text 404 for any other method.
  - **Go bug fixed:** for a public link with a missing or wrong `h`, Go renders the page in the handler and again
    in `handleContextError`, so the 400 body holds two pages. Rust sends one.
- **No user limit.** An unlicensed 12.1 server refuses to create users past 250 (`maxUsersHardLimit`,
  `channels/app/limits.go`); production has 1,200. The Rust server has no such limit. A Go server taking its turn
  on the database keeps working for existing users, but cannot add any: build it from source without the limit if
  that is ever needed.
- **Push and email** are sent as Go sends them: see `docs/NOTIFICATIONS.md`.
- **Query strings** are read as Go's `r.URL.Query()` by every handler (`app::Query`): a pair with a raw `;` or a bad
  escape is dropped, a repeated key gives its first value.
- **`WebserverMode`** (read at start): "gzip" compresses the static, plugin and docs files and the handlers below;
  "uncompressed" serves them plain (no `Vary`); "disabled" serves no web client route at all (`/static`, `/robots.txt`,
  `/unsupported_browser.js`, `/_documentation`, the client's page): Go's plain-text 404 for any path outside `/api/`.
- **Compression** (`src/compress.rs`): with `WebserverMode` "gzip" (read at start), every API and web handler Go
  wraps in `gzhttp` (klauspost/compress 1.19.2) answers as it does: `Vary: Accept-Encoding` always; zstd or gzip
  by the `Accept-Encoding` q-values, zstd on a tie (browsers offering both get zstd), never for `HEAD`; only bodies
  of 1 KiB or more (streamed bodies are buffered up to that), without `Content-Encoding`/`Content-Range`, of a
  type not already compressed (not audio, video, JPEG or an archive); `Content-Length` and `Accept-Ranges` go.
  Not wrapped, as in Go: the API's 404 for an unknown path, `root.html`, `robots.txt`, `unsupported_browser.js`,
  and a websocket upgrade (101). Compressed bytes differ from Go's; they decompress to the same body.
- **Channel mentions** (`props.channel_mentions` of a post, `posts.rs`): stored as Go's `FillInPostProps` writes
  them (display name, team name, channel id); every answer gives the viewer only the mentions they may resolve,
  looked up again by team and channel name, with the current display name and without the id
  (`sanitizeChannelMentionsForUser`, also inside permalink previews); `posted` and `post_edited` events carry the
  post without them and add, per recipient, the stored form of the mentions whose channel the recipient may
  resolve (`channelMentionsBroadcastHook` with `HasPermissionToResolveChannelMention`: read content, or a public
  channel of a team where they may read public channels). Edits look the mentions up in the post's own team.
- **`HEAD` on the API** is Go's 404 (its routes are registered for their methods only), but for the file routes
  (`/files/{id}`, `/thumbnail`, `/preview`) that take it.
- **Open:**
  - bots: about 116 Lambdas (`node` UA) do email lookup → `channels/direct` → `posts`, and there are incoming
    webhooks. These are T2, but critical.

## ORM: what the prototype showed

The stack is `sqlx` (Postgres) and our own `#[model]` attribute (`macros/`). It is not a full ORM:

- **One struct per table.** It is both the row and the API JSON. The column name is derived (`create_at` →
  `createat`). The macro generates `SELECT`, `INSERT` and `UPDATE` by id, plus `FromRow`.
- **Go's JSON rules:**
  - `#[mm(omitempty)]`;
  - missing fields and `null` decode to zero values;
  - map keys sorted (`BTreeMap`);
  - field order as in Go.
- **NULL columns read as zero values.** The schema is almost entirely nullable. Plain `sqlx::FromRow` fails on that
  for non-`Option` fields; Go's stores never see NULL.
- **Go's storage types:** `StringMap` and `StringInterface` in jsonb, `StringArray` as JSON text in varchar.

SeaORM and Diesel would need a second struct per table, because their column names are not the JSON names. Complex
queries (unreads, threads, sidebar, search) stay as SQL strings, ported from `sqlstore` and fixed where they scan.

`tests/compat_model.rs` checks this against the Go server on the same database:

- **Read:** a `Users` row read by Rust serialises to exactly what Go sends for `GET /users/me`, keys and order.
- **Write:** a post inserted by Rust comes back from Go's `GET /posts/{id}` with every field equal.

Found on the way: Go answers `/users/me` from its user cache, one login behind the database's `UpdateAt`. Caches in Go
are a risk for taking turns on one database: switch servers with a restart, never run both at once.

## Status

**First milestone reached: EBAChat works against the Rust server.**

- **Routes:** every T1 route is ported. So are the T2 routes that matter in production:
  - incoming webhooks, which bots and Jira use;
  - mobile session and device routes;
  - `channel_members` and the other bootstrap calls of the stock clients.
- **Plugins and licensed features:** they answer as a server without plugins or a license does.
- **`tools/diff/diff.py`:** runs EBAChat's calls, in the client's order, against Go and Rust. It compares status,
  keys, key order and values. For writes it also compares the websocket events an observer receives.
  All 61 calls answer as Go does.
- **The real client:** EBAChat ran against the Rust server, as a test copy with its own session file
  (`EBACHAT_SESSION`), and the server logged every request (`LOG_REQUESTS=1`).
  - Startup, the websocket, and posts, threads, reactions, edits, deletes and DMs from another user all worked.
  - Every answer was 2xx, and the client logged no errors.
- **Caches:** Go's caches are reproduced, see `docs/CACHES.md`.
- **Server busy** (`POST /server_busy`): as Go, the ten `DisableWhenBusy` routes (user, post, team, channel and
  file searches, REST typing) answer 503 `api.context.server_busy.app_error` after the session check,
  `status_change` is not broadcast and the websocket `user_typing` action fails until the expiry. One atomic load
  per check.
- **Properties:** see `docs/PROPERTIES.md`.
- **Push and email:** `docs/NOTIFICATIONS.md`.
- **Security review fixes** (SMTP injection and STARTTLS downgrade, SSRF at connect time and own addresses,
  websocket origin, MFA lockout and replay, the public link salt): `docs/SECURITY.md`.
- **Background jobs:** Go's job server, the Team Edition workers and schedulers, the recurring cleanups, content
  extraction, bulk export and bulk import (every JSONL line type, rows as Go writes them); see `docs/JOBS.md`.
- **Left:**
  - T3 routes on demand.

Code by area:

| Area | Files |
| --- | --- |
| Core | `app.rs`, `cache.rs`, `perm.rs`, `config.rs`, `hub.rs`, `status.rs`, `db.rs`, `model/` |
| Users and teams | `users.rs`, `user_*.rs`, `teams.rs`, `preferences.rs`, `emoji.rs`, `system.rs`, `i18n.rs` |
| Channels | `channels.rs`, `channels_api.rs`, `sidebar.rs` |
| Notifications | `notify.rs`, `push.rs`, `email.rs`, `mail.rs`, `email_template.rs`, `markdown.rs` |
| Posts | `posts.rs`, `notify.rs`, `threads.rs`, `reactions.rs`, `drafts.rs`, `search.rs` |
| Files | `files.rs`, `file_api.rs`, `images.rs`, `opengraph.rs`, `search_params.rs` |
| Integrations | `commands.rs`, `commands_builtin.rs`, `integrations.rs` (actions, dialogs), `webhooks.rs`, `reminders.rs` |
| Plugins | `plugins/` (manager, API, routes, key-value store; see `docs/PLUGINS.md`), the `goplugin` crate (protocol) |
| Jobs | `jobs.rs`, `job_workers.rs`, `extract.rs`, `bulk_export.rs`, `bulk_import.rs`, `import_functions.rs` |
| Admin | `admin.rs`, `audit.rs` (Audits rows), `logs.rs` (server log, `/logs`, startup lines), `license.rs`, `permanent.rs`, `cpa.rs`, `config_order.rs`; see `docs/ADMIN.md` |
| HTTP | `api.rs` (router, Go's headers, `HEAD`), `compress.rs` (gzhttp), `web.rs` |

## Running

```sh
make stand                      # Postgres (and Go) of the stand
./dev/run-go.sh &               # Go 12.1.0 on :8065
cargo build --release && ./dev/run-rust.sh   # Rust on :8066, the same config and dev/stand.env
python3 tools/diff/diff.py --writes          # Go and Rust answer the same?
```

Never run both servers on one database outside the stand. Each keeps its own caches, and neither sees the other's
writes in them.
