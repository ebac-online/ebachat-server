# The server alone. The client is not in the image: mount one at /ebachat/client (EBACHAT_CLIENT_DIR). Settings come
# from a mounted config.json.
FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin ebachat-server && cp target/release/ebachat-server /ebachat-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /ebachat
COPY --from=build /ebachat-server /usr/local/bin/ebachat-server
COPY i18n i18n
COPY templates templates
ENV MM_CONFIG=/ebachat/config/config.json LISTEN=0.0.0.0:8065
EXPOSE 8065
CMD ["ebachat-server"]
