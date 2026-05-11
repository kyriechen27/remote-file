FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/remote-file /usr/local/bin/remote-file

ENV REMOTE_FILE_BIND=0.0.0.0:8080 \
    REMOTE_FILE_ROOT=/data/files \
    REMOTE_FILE_DATA=/data/meta

RUN mkdir -p /data/files /data/meta

EXPOSE 8080

CMD ["remote-file"]
