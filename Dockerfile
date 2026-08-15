# MDA server image (PLAN §3). Multi-stage build keeps the final image small.

FROM rust:1-bookworm AS builder
WORKDIR /mda
# Build dependencies first (better layer caching).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
RUN cargo build --release --bin mda-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --user-group --no-create-home mda
WORKDIR /app
COPY --from=builder /mda/target/release/mda-server /app/mda-server
COPY --from=builder /mda/migrations /app/migrations
# Writable blob root (LocalBlobStore, §5.14) owned by the runtime user.
RUN mkdir -p /var/lib/mda/blobs && chown -R mda:mda /var/lib/mda /app
USER mda:mda
EXPOSE 8080
ENV MDA_HOST=0.0.0.0 MDA_PORT=8080 LOG_FORMAT=json \
    MDA_BLOB_DIR=/var/lib/mda/blobs
CMD ["/app/mda-server"]
