# Build the gateway binary. protoc is needed to compile the tapd proto subset.
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN cargo build --release --bin ozark-gateway

# Slim runtime. rusqlite is bundled (static sqlite) and rustls uses ring (static),
# so the only runtime need is CA certs.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/ozark-gateway /usr/local/bin/ozark-gateway

# Listen on all interfaces inside the container so the Tor sidecar can reach it.
ENV OZARK_GATEWAY_LISTEN=0.0.0.0:8787
ENV OZARK_GATEWAY_DB=/data/ozark-gateway.sqlite
EXPOSE 8787
ENTRYPOINT ["ozark-gateway"]
