# Dev stand

- `docker-compose.yml`: Postgres 16 (:5433) and Mattermost Team Edition 12.1.0 (:8065, amd64 under emulation).
- `run-go.sh`: the Go server built natively instead of the container, on the same database and port. Stop the `mm`
  container first: never two servers on one database at once.
- `bin/`, `run/`: binaries, logs, tokens, benchmark output; not tracked.

## Building the Go server natively

```sh
cat > /tmp/go.work <<W
go 1.26.10
use (
	$PWD/../ebac-messenger/mattermost/server
	$PWD/../ebac-messenger/mattermost/server/public
)
W
cd ../ebac-messenger/mattermost/server
GOWORK=/tmp/go.work go build -buildvcs=false -o ../../../ebachat-server/dev/bin/mattermost \
  -ldflags '-X "github.com/mattermost/mattermost/server/public/model.BuildNumber=12.1.0"
            -X "github.com/mattermost/mattermost/server/public/model.BuildEnterpriseReady=false"
            -X "github.com/mattermost/mattermost/server/public/model.BuildHash=243e3d0135203ab9166fe8f2873b07f697b06fd4"' \
  ./cmd/mattermost
```

No build tags: that is Team Edition. Without the `go.work` the build picks `server/public` from the module cache and
fails.

## Docker

```sh
docker build -t ebachat-server .
docker run -d --name ebachat-server -p 8066:8065 \
  -v <config>:/ebachat/config -v <client>:/ebachat/client:ro -v <files>:/ebachat/data \
  -e 'DATABASE_URL=postgres://<user>:<password>@<host>:5432/<database>?sslmode=disable' ebachat-server
```

`<config>` is a folder with `config.json`, writable: the server writes it back at start.
