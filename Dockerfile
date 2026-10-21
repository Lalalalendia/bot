FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml ./
COPY src/ ./src/
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 10001 -m bot \
    && mkdir /data && chown bot:bot /data
COPY --from=builder /src/target/release/rebuild-bot /usr/local/bin/rebuild-bot
USER bot
ENV DATABASE_PATH=/data/rebuild.sqlite3
ENTRYPOINT ["/usr/local/bin/rebuild-bot"]
