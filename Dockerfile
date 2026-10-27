# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
COPY --from=builder /app/target/release/docker-dash /usr/local/bin/docker-dash
ENV DOCKER_DASH_BIND=0.0.0.0:8080
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/docker-dash"]
