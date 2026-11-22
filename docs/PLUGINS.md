# Plugins

Server plugins run as they are: the plugin bundles (jira, gcal, remind, alertmanager...) are started as their own
processes and talk to the server over the go-plugin protocol (net/rpc over yamux, gob encoded). Code:
`goplugin/` (the protocol: gob, net/rpc, the stream broker, the plugin process), `src/plugins/` (manager, API,
routes, key-value store).

## Running them

- **Where:** `PluginSettings.Directory` (`./plugins`, so `/ebachat/plugins` in the image), one extracted bundle per
  directory with its `plugin.json`. The server executable for the OS and architecture comes from
  `server.executables` (or `server.executable`) and must be executable.
- **Which:** with `PluginSettings.Enable`, every bundle whose `PluginSettings.PluginStates.<id>.Enable` is true.
  `mattermost-ai`, `com.mattermost.calls`, `playbooks` and `focalboard` are never started.
- **Lifecycle:** started at boot and on every config change that enables one; stopped (`OnDeactivate`, then the
  process) when disabled or gone. `OnConfigurationChange` goes to the active plugins on every config save.
- **Health:** every 30 s each plugin is pinged; a dead or silent one is restarted, and left stopped
  (`FailedToStayRunning`) after its third failure within an hour.
- **Logs:** the plugin's hclog lines and its API log calls go to the server log with `plugin_id`.
- **Environment:** a plugin gets only `PATH`, `HOME`, `TZ`, the locale, `TMPDIR`, the CA and proxy variables.

Installing is copying the extracted bundle into the directory and enabling it (System Console or
`POST /api/v4/plugins/{id}/enable`). Uploads, install from URL, the marketplace and removal answer 501.

## What they get

- **API:** the methods jira, gcal, remind and alertmanager call: logs, configuration (`LoadPluginConfiguration`
  merges the manifest's defaults under the saved settings), key-value store, users, statuses, preferences, bots
  (`EnsureBotUser`), teams, channels, posts and ephemeral posts, reactions, files, slash commands, websocket events
  (`custom_<id>_<event>`), interactive dialogs. Any other method answers an error in its `*AppError` or `error`
  result, and the server logs it once per plugin and method ("Plugin called an API method this server does not
  implement"). Plugins have no direct database access.
- **HTTP:** `/plugins/{id}/...` goes to the plugin's `ServeHTTP` with `/plugins/{id}` cut. A valid session (token,
  cookie or `access_token`) sets `Mattermost-User-Id`; a cookie session on anything but GET needs the CSRF token
  (`X-CSRF-Token` or a `csrf` form field, or `X-Requested-With: XMLHttpRequest` unless strict). The token, the
  session cookie, `Referer` and `Mattermost-User-Id` sent by the client never reach the plugin.
  `/plugins/{id}/public/...` serves the bundle's `public/` files.
- **Webapp:** `/api/v4/plugins/webapp` lists the active plugins' client manifests; the bundle is served from the
  plugin directory as `/static/plugins/{id}/{id}_{fnv64a of the bundle}_bundle.js`, the rest of its directory
  beside it.
- **Slash commands:** a plugin's commands run before custom and built-in ones, with the message's user and channel
  mentions; they are listed in `/commands` and autocomplete. Dynamic list arguments are fetched from the plugin.
- **Actions, dialogs, lookups:** URLs under `/plugins/` are requests to the plugin for the clicking user.
- **Management:** `/api/v4/plugins` (installed, active and inactive), `/plugins/statuses`, `/plugins/{id}/enable`,
  `/plugins/{id}/disable`; `plugin_enabled`, `plugin_disabled` and `plugin_statuses_changed` events.

Plugin settings in `GET /api/v4/config` are those of installed plugins, secret ones masked; a masked value saved
back keeps the stored one.

## The method table

`src/plugins/schema_gen.rs` lists every API method and hook with the types of its arguments and results, and the
structs they carry (field, JSON name, type). It is generated from the plugin and model packages of the server's
public module, at the version plugins are built against:

```sh
python3 tools/plugin_schema.py <checkout>/server/public > src/plugins/schema_gen.rs
```

Arguments arrive as gob values and are read as this server's JSON by the table; results are written back the
same way. A field a plugin leaves out is its zero value.
