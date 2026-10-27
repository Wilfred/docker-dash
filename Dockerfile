# syntax=docker/dockerfile:1
FROM docker:29-cli AS docker-cli

FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && install -d -o 10001 -g 10001 /home/docker-dash
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=builder /app/target/release/docker-dash /usr/local/bin/docker-dash
ENV HOME=/home/docker-dash \
    DOCKER_DASH_BIND=0.0.0.0:8080
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/docker-dash"]
