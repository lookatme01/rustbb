FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY migrations migrations
COPY templates templates
COPY static static
COPY lang lang
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates postgresql-client && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 rbb && mkdir -p /app/uploads /app/plugins && chown -R rbb /app
WORKDIR /app
COPY --from=build /src/target/release/rbb /app/rbb
USER rbb
ENV RBB_LISTEN=0.0.0.0:8080 RBB_UPLOAD_DIR=/app/uploads RBB_PLUGINS_DIR=/app/plugins
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080"]
CMD ["/app/rbb", "serve"]
