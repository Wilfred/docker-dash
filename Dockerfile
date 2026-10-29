# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
COPY templates ./templates
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
COPY --from=builder /app/target/release/docker-dash /usr/local/bin/docker-dash
USER 10001:10001
EXPOSE 8080
# Graceful shutdown listens for SIGINT; PID 1 ignores an unhandled SIGTERM.
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/docker-dash"]
