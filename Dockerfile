FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src src
COPY migrations migrations
COPY templates templates
COPY static static
COPY lang lang
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates postgresql-client && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 rbb && mkdir -p /app/uploads /app/plugins && chown -R rbb /app
WORKDIR /app
COPY --from=build /src/target/release/rbb /app/rbb
USER rbb
ENV RBB_LISTEN=0.0.0.0:8080 RBB_ADMIN_LISTEN=0.0.0.0:9090 RBB_UPLOAD_DIR=/app/uploads RBB_PLUGINS_DIR=/app/plugins \
    RBB_LOG_JSON=true RBB_SHUTDOWN_DRAIN_SECS=5
EXPOSE 8080 9090
# Readiness through the admin listener: works for web, worker and scheduler containers alike.
HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 CMD ["/app/rbb", "healthcheck"]
STOPSIGNAL SIGTERM
CMD ["/app/rbb", "serve"]
