# docker-dash

A minimal, server-rendered Rust web UI for inspecting Docker containers on a
single host. Designed for fewer than ten containers and for agents investigating
production issues. Pages contain complete text data and ordinary links; no
JavaScript is required.

For each container, you can see the image, version, uptime and logs
(latest or paginated history), plus the Docker version and uptime of
the box itself.

## Run

Requires a current stable Rust toolchain (edition 2024), the Docker CLI on
`PATH`, and permission to query a running local Docker daemon:

```sh
docker version
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
The application only issues read commands (`version`, `ps`, `container inspect`,
`image inspect`, and `logs`); access to the Docker socket itself still grants
broad host privileges.

## Dashboard behavior

- Lists all containers, including created, running, paused, and exited ones,
  with their name, ID, image reference, image tag or digest, and status.
  Uptime is shown only for running/paused containers; stopped containers show
  their last start/finish timestamps and exit code, and never-started containers
  are marked explicitly. A tag is the image's
  declared version, not a detected application version. Untagged names show
  `latest (implicit)`.
- Shows the Docker **server** version and Linux host uptime from `/proc/uptime`.
  Host uptime displays as unavailable on systems without that file.
- Shows each container image's build/creation time in UTC, using `Created` from
  `docker image inspect` against the container's immutable image ID. This is
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
  cannot be determined from CLI output.
- Docker commands time out after 15 seconds. Output is limited to 4 MiB per
  stream per command; unusually large logs produce an error rather than being
  silently truncated. Docker connection errors and unsupported logging drivers
  appear in the UI.

The Docker CLI inherits its usual context and environment, including
`DOCKER_HOST`. Use a local Docker context to match this application's intended
scope: host uptime always describes the machine running docker-dash, even if
the CLI points at a remote daemon.

## Agent access

The HTML overview (`/`) and log pages (`/containers/<id>/logs`) include their data
in the response body, so HTTP clients and agents can read them without a browser
runtime. Log pagination uses ordinary links that keep the snapshot fixed.

For structured data:

```sh
curl --fail http://127.0.0.1:8080/api/overview
curl --fail "http://127.0.0.1:8080/api/containers/CONTAINER_ID/logs"
```

Replace `CONTAINER_ID` with a container ID from the overview (and quote the URL in a
shell if using query parameters). The overview includes `checked_at`,
`docker_version`, `host_uptime_seconds`, and `containers`. Each container includes
`id`, `name`, `image`, `version`, `image_id`, `image_created_at` (UTC), `status`,
`started_at`, `finished_at`, `exit_code`, and `uptime_seconds`. Lifecycle
fields are `null` when not applicable (for example, uptime for exited containers
or start time for containers that have never started).
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

Tests use a simulated Docker executable and exercise container metadata, empty
hosts, merged and paginated logs, snapshot isolation, input validation, embedded
assets, and Docker failures without requiring daemon access. CLI fixture tests
run on Unix platforms.

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

The image contains the Docker CLI and runs as UID/GID `10001:10001`.
`--group-add` grants access using the socket's host group. A read-only bind mount
still allows Docker API calls; it does not restrict Docker socket privileges.
For rootless Docker, substitute its socket path and group. Host uptime inside
the image comes from the container's `/proc/uptime`, which reflects the Linux
kernel host (the Docker VM on Docker Desktop).

To build the image locally:

```sh
docker build --tag docker-dash:local .
```
