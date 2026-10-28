# docker-dash

A read-only Docker dashboard for small hosts (fewer than ten containers),
designed for agents investigating production issues.

## Usage

On a Linux Docker host:

```sh
sudo docker run -d --name docker-dash --restart unless-stopped \
  --network host \
  -e DOCKER_DASH_BIND=127.0.0.1:8080 \
  --group-add "$(stat -c '%g' /var/run/docker.sock)" \
  -v /var/run/docker.sock:/var/run/docker.sock:ro \
  ghcr.io/wilfred/docker-dash:latest
```

Open **http://127.0.0.1:8080**. Click a container for diagnostics or follow its
logs link. Resource usage is available from the navigation bar.

To share through Tailscale:

```sh
tailscale serve --bg http://127.0.0.1:8080
```

The app has no login; restrict access through your tailnet's access policy.

Agents can read the HTML directly or use `/api/overview`, `/api/usage`,
`/api/containers/<id>`, and `/api/containers/<id>/logs` for JSON.

To run from source, install Rust and run `cargo run --release` with permission
to access `/var/run/docker.sock`.

Format CSS with Node.js installed:

```sh
npx --yes prettier@3.8.1 --write 'static/**/*.css'
```

CI checks CSS formatting with the same Prettier version.

## Design principles

- Text first: complete HTML, ordinary links, and no JavaScript requirement.
- A compact home page, with diagnostics and paginated logs on separate pages.
- Show all containers, including stopped ones, and image build times.
- Read-only access directly through the local Docker socket.
- One behavior: no feature switches or background polling.
- Expose selected diagnostics, not environment variables, commands, or raw
  inspection data. Mask known secrets in logs; unknown secrets may still appear.
