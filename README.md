# docker-dash

A minimal, server-rendered Rust web UI for inspecting Docker containers on a
single host. Designed for fewer than ten containers and for agents investigating
production issues. Pages contain complete text data and ordinary links; no
JavaScript is required.

For each container, you can see the image, version, uptime and logs
(latest or paginated history), plus the Docker version and uptime of
the box itself.

## Run

Requires a current stable Rust toolchain (edition 2024), a Unix host, and
permission to access a running local Docker daemon's Unix socket. The application
uses the Docker Engine HTTP API directly; no Docker CLI or subprocesses are used:

```sh
cargo run --release
```

Open **http://127.0.0.1:8080**. The binary embeds its HTML and CSS;
there is no Node build step or external asset service. Stop it with Ctrl+C.

To use another listen address:

```sh
DOCKER_DASH_BIND=127.0.0.1:9090 cargo run --release
```

The dashboard has no authentication. Keep its default loopback binding or put it
behind an authenticated reverse proxy before exposing it to other machines.
The application only issues GET requests to the Docker Engine API (version,
info, container/image inspection, stats, and logs). Access to the Docker socket
itself still grants broad host privileges; the dashboard is not a socket proxy.

## Dashboard behavior

- Lists all containers, including created, running, paused, and exited ones,
  with their name, ID, image reference, image tag or digest, and status.
  Uptime is shown only for running/paused containers; stopped containers show
  their last start/finish timestamps and exit code, and never-started containers
  are marked explicitly. A tag is the image's
  declared version, not a detected application version. Untagged names show
  `latest (implicit)`; image IDs without a repository tag show `untagged`.
- Shows published port bindings, configured bindings on inactive containers,
  exposed-only ports, network mode, network attachments, and container IPs.
  Host networking shares host ports; Docker does not report those listening
  sockets. Exposed ports describe configuration, not proof of a listener.
- Shows health status/failure count, OOM kills, restart count/policy, sanitized
  Docker state errors, container creation time, logging driver, and memory,
  CPU and process limits.
- **Resource usage** (`/usage`, `/api/usage`) samples running containers on
  demand: CPU, memory, process count, and network/block I/O counters. It does
  not poll in the background. CPU percentages use Docker CPU/system deltas
  and online CPU count; memory excludes inactive file cache on Linux. The JSON
  includes formatted units and numeric CPU/memory values.
- Shows the Docker host name, OS, kernel, architecture, CPU count and memory
  from the Engine info endpoint (selected fields only).
- Shows the Docker **server** version and Linux host uptime from `/proc/uptime`.
  Host uptime displays as unavailable on systems without that file.
- Shows each container image's build/creation time in UTC, using `Created` from
  the image inspection API against the container's immutable image ID. This is
  image metadata, not the time it was pulled or the container was started, and
  does not depend on where a mutable tag such as `latest` points now. Missing
  or invalid timestamps display as unavailable (JSON `null`). Removed images
  also display as unavailable without hiding their containers.
- Shows full container IDs, UTC timestamps, and uptime including exact seconds.
  Use **Refresh** to fetch a new overview; pages do not change while being read.
- **Logs** captures recent stdout and stderr, merged by Docker timestamps.
  Each page contains up to 100 lines, in chronological order. **Older** and
  **Newer** navigate the captured history; **Latest** captures fresh output.
  Log content is displayed as plain text.
- Snapshots hold up to 10,001 lines, with 100 pages (10,000 lines) accessible.
  They expire after five minutes, or earlier when eight newer snapshots are
  captured. Select **Latest** if a snapshot expires. Rotation can remove logs
  before capture; the dashboard cannot recover logs Docker no longer retains.
  Equal timestamps preserve stream order; their order across stdout and stderr
  is not distinguished when timestamps are identical.
- Docker API requests time out after 15 seconds. Responses are limited to
  4 MiB per request; unusually large logs produce an error rather than being
  silently truncated. Docker connection errors and unsupported logging drivers
  appear in the UI. At most four Docker API requests run concurrently; requests
  waiting longer than five seconds receive a retryable HTTP 503.

The default socket is `/var/run/docker.sock`. To select another local socket
(for example rootless Docker or Docker Desktop on macOS), set an absolute path:

```sh
DOCKER_DASH_SOCKET=/run/user/1000/docker.sock cargo run --release
```

API versions 1.40–1.56 are supported, negotiated against the daemon's reported
range. Docker contexts, `DOCKER_HOST`, TCP/TLS endpoints, and Windows named pipes
are not used. Host uptime always describes the machine running docker-dash;
the OS/kernel/CPU/memory fields describe the Docker daemon host.

## Tailnet sharing and sensitive data

Keep the dashboard bound to loopback, then expose it to your tailnet using
[Tailscale Serve](https://tailscale.com/docs/reference/tailscale-cli/serve):

```sh
tailscale serve --bg http://127.0.0.1:8080
tailscale serve status
```

Use the HTTPS URL reported by Serve for agent access, including `/api/overview`
and `/api/usage`. Restrict access to the intended users/devices through your
Tailscale access policy. The app has no login and does not treat forwarded
identity headers as authentication. Serve provides the network access boundary;
Funnel would publish it outside the tailnet and is not needed here. The commands
above configure Serve at `/`; mounting under a subpath is not supported.

When running the image with host networking, keep it on loopback too:

```sh
docker run --detach --name docker-dash \
  --network host \
  --env DOCKER_DASH_BIND=127.0.0.1:8080 \
  --group-add "$(stat -c '%g' /var/run/docker.sock)" \
  --volume /var/run/docker.sock:/var/run/docker.sock:ro \
  ghcr.io/wilfred/docker-dash:latest
```

Inspection responses use an explicit field allowlist. Environment variables,
commands/arguments, labels, health-check commands/output, mount sources,
logging options, registry credentials, and raw Docker inspection documents are
not exposed. Logs and Docker state errors mask exact values from environment
variables named for secrets (keys, tokens, passwords, credentials, auth,
cookies, DSNs, database URLs), plus passwords extracted from connection URLs.
Masking occurs before snapshots are cached and applies to HTML and JSON.

This is best-effort masking, not a guarantee that arbitrary log/error messages
are secret-free: encoded values, file-based secrets, unusually named variables,
and unrelated credentials may remain. Treat log access as privileged. To share
metadata and usage without exposing application logs, set:

```sh
DOCKER_DASH_LOGS=false cargo run --release
# For the image, add: --env DOCKER_DASH_LOGS=false
```

Log links and free-form Docker state errors are removed; both HTML and JSON log
endpoints return HTTP 403,
including requests for existing snapshots. `DOCKER_DASH_LOGS` accepts only
`true` or `false`; the default is `true`. Responses are uncached, cannot be
framed, forbid scripts, omit referrers, and ask crawlers not to index them.

## Agent access

The HTML overview (`/`) and log pages (`/containers/<id>/logs`) include their data
in the response body, so HTTP clients and agents can read them without a browser
runtime. Log pagination uses ordinary links that keep the snapshot fixed.

For structured data:

```sh
curl --fail http://127.0.0.1:8080/api/overview
curl --fail http://127.0.0.1:8080/api/usage
curl --fail "http://127.0.0.1:8080/api/containers/CONTAINER_ID/logs"
```

Replace `CONTAINER_ID` with a container ID from the overview (and quote the URL in a
shell if using query parameters). The overview includes `checked_at`,
`docker_version`, `host` (Docker host details), `host_uptime_seconds`,
`logs_enabled`, and `containers`. Each container includes
`id`, `name`, `image`, `version`, `image_id`, `image_created_at` (UTC), `status`,
`started_at`, `finished_at`, `exit_code`, and `uptime_seconds`. Lifecycle
fields are `null` when not applicable (for example, uptime for exited containers
or start time for containers that have never started). Additional fields include
`created_at`, `state_error`, `healthcheck_configured`, `health_status`,
`health_failing_streak`, `oom_killed`,
`restart_count`, `restart_policy`, `restart_max_retries`, `network_mode`, `ports`
(each with `container_port`, `published` and `configured` bindings), `networks`,
`memory_limit_bytes`, `nano_cpus`, `cpu_quota`, `cpu_period`, `pids_limit`, and
`log_driver`. Usage responses also include `cpu_percent_value`, `memory_usage_bytes`, and
`memory_limit_bytes`, `memory_percent_value`, `network_rx_bytes`,
`network_tx_bytes`, `block_read_bytes`, `block_write_bytes`, `pids_current`,
and the per-container `sampled_at` timestamp; missing samples are null instead of NaN or infinity.
Zero memory/CPU limits mean no explicit Docker limit; process
limits of zero/negative mean unlimited, and null means the Docker default.
Logs return `lines`, `page` (zero-based), `until` (the snapshot token), `has_older`,
and `history_limit_reached`. Follow the HTML page's **This page as JSON** link,
or pass the returned `until` and increment `page` to fetch older history.
Expired snapshots return HTTP 410; omit `until` and use page 0 to capture again.
Errors return non-2xx status codes and a JSON `error` string on API routes.

## Develop and verify

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Integration tests run a mock Docker Engine HTTP server over temporary Unix
sockets. They verify version negotiation, read-only requests, all container
states, missing images, metadata allowlists, secret masking and cached pages,
TTY and multiplexed logs (including split UTF-8), response limits, usage
calculations and HTML rendering without requiring a Docker daemon. The CI
smoke test also exercises the image against a real Docker socket.

## Container image

GitHub Actions runs formatting, Clippy (warnings are errors), and tests on pull
requests and pushes to `main`. After all checks and an image smoke test pass,
`main` publishes to GitHub Container Registry:

- `ghcr.io/wilfred/docker-dash:latest`
- `ghcr.io/wilfred/docker-dash:sha-<full-commit-sha>`

Publishing uses the workflow's `GITHUB_TOKEN` with `packages: write`; no registry
password secret is needed. Actions are pinned to commit hashes. Pull requests
build and smoke-test the image without logging in or publishing. The first
published package may be private; change its visibility in GitHub Packages if
anonymous pulls are wanted, or authenticate with `docker login ghcr.io`.

Run on a Linux Docker host:

```sh
docker run --detach --name docker-dash \
  --publish 127.0.0.1:8080:8080 \
  --group-add "$(stat -c '%g' /var/run/docker.sock)" \
  --volume /var/run/docker.sock:/var/run/docker.sock:ro \
  ghcr.io/wilfred/docker-dash:latest
```

The image runs as UID/GID `10001:10001` and contains no Docker CLI.
`--group-add` grants access using the socket's host group. A read-only bind mount
still allows Docker API calls; it does not restrict Docker socket privileges.
For rootless Docker, substitute its socket path and group. Host uptime inside
the image comes from the container's `/proc/uptime`, which reflects the Linux
kernel host (the Docker VM on Docker Desktop).

To build the image locally:

```sh
docker build --tag docker-dash:local .
```
