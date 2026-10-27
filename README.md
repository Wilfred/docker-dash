# docker-dash

A simple Rust web UI showing basic information about the Docker containers
running on the current host.

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

Open **http://127.0.0.1:8080**. The binary embeds its HTML, CSS, and JavaScript;
there is no Node build step or external asset service. Stop it with Ctrl+C.

To use another listen address:

```sh
DOCKER_DASH_BIND=127.0.0.1:9090 cargo run --release
```

The dashboard has no authentication. Keep its default loopback binding or put it
behind an authenticated reverse proxy before exposing it to other machines.
The application only issues read commands (`version`, `ps`, `container inspect`,
and `logs`); access to the Docker socket itself still grants broad host privileges.

## Dashboard behavior

- Lists running containers with their name, ID, image reference, image tag or
  digest, status, and time since the most recent start. A tag is the image's
  declared version, not a detected application version. Untagged names show
  `latest (implicit)`.
- Shows the Docker **server** version and Linux host uptime from `/proc/uptime`.
  Host uptime displays as unavailable on systems without that file.
- Refreshes the overview every 15 seconds while the tab is visible. Filter by
  name, image, or tag, or use **Refresh** to update immediately.
- **View logs** captures recent stdout and stderr, merged by Docker timestamps.
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
