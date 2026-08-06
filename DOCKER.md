# Deploying with Docker

This guide covers running the crypto payment processor in Docker, either from a
prebuilt image or by building from source.

Repository: <https://github.com/AntCDev/crypto-payments>

---

## Concepts

- An **image** is a frozen snapshot built from the `Dockerfile`: the compiled binary
  plus `wwwroot`, on a minimal Linux base.
- A **container** is a running instance of an image.
- `docker-compose.yml` starts multiple containers together — here, the application
  and its Postgres database — with a single command.

---

## Requirements

- Docker Engine with the Compose plugin (`docker compose`, not the legacy
  `docker-compose` binary).
- Nothing else. Rust and Node are not required on the host; both toolchains live
  inside the build stages of the image.

---

## Quick start

``` bash
git clone https://github.com/AntCDev/crypto-payments.git
cd crypto-payments
cp .env.example .env
# edit .env — see the configuration reference below
docker compose up --build
```

This builds the image from `Dockerfile`, starts Postgres, waits for it to report
healthy, then starts the application. The application is then available at
`http://localhost:8080`, or whatever `PORT` is set to in `.env`.

The first build is slow because every crate is compiled from scratch. Subsequent
rebuilds after code-only changes are much faster thanks to dependency layer
caching (see [Build pipeline notes](#build-pipeline-notes)).

`Dockerfile`, `.dockerignore`, and `docker-compose.yml` all live in the project
root, next to `Cargo.toml`.

---

## Configuration

All configuration is supplied at **run** time through `.env`. Copy the template
and fill it in:

``` bash
cp .env.example .env
```

### Minimum required values

```
PORT=8080
HOST=0.0.0.0
POSTGRES_USER=cryptopay
POSTGRES_PASSWORD=<choose a password>
POSTGRES_DB=cryptopay
MASTER_KEY=<generated master key>
```
### `DATABASE_URL` vs. the `POSTGRES_*` components

These two blocks serve different run modes and are not used at the same time.

- **Standalone (`cargo build && cargo run`, no Docker):** set `DATABASE_URL` to a
  full connection string pointing at an existing Postgres instance.
- **Docker:** leave `DATABASE_URL` alone and set `POSTGRES_USER`,
  `POSTGRES_PASSWORD`, and `POSTGRES_DB` instead. `docker-compose.yml` reads those
  three values, composes a `DATABASE_URL` from them, and **overrides** whatever is
  in `.env` so the connection points at the `db` container rather than
  `localhost`.

### `HOST`

Inside a container, set `HOST=0.0.0.0`. Binding to `127.0.0.1` makes the
application unreachable from outside the container, which is the most common
cause of a hanging or refused `curl` (see [Troubleshooting](#troubleshooting)).

Binding `0.0.0.0` inside a container is safe: Docker's network isolation and the
explicit `ports:` mapping in `docker-compose.yml` are what control real-world
exposure, not the bind address.

### RPC URLs and contract addresses

These are optional. Without them the service still builds and boots — networks
lacking a valid RPC URL simply log `❌ No valid RPC_URL found` at startup and
their tokens are not registered as available. Supply the RPC URLs and contract
addresses for whichever chains need to be live.

---

## Two ways to run it

A single multi-stage `Dockerfile` covers both audiences. There is no need to
publish separate "prebuilt" and "source" images — the stages already draw that
line:

```
frontend-builder  →  compiles frontend/ into wwwroot/
chef / planner    →  resolves the dependency graph
builder           →  compiles src/ into target/release/<binary>
runtime           →  copies ONLY the binary + wwwroot into a clean, minimal base
```

### Run a prebuilt image

Operators who just want the service running pull a published image and never
touch Rust or Node:

``` bash
docker pull ghcr.io/antcdev/crypto-payments:latest
docker run -p 8080:8080 --env-file .env ghcr.io/antcdev/crypto-payments:latest
```

### Build from source

Operators who want to customize clone the repository — which *is* the full source
— and build their own image:

``` bash
docker build -t my-org/crypto-payments .
```

The same `Dockerfile` runs the frontend build and the Rust build locally and
produces an image with their customizations baked in.

### Interactive toolchain shell

The `builder` stage can be targeted on its own, for poking around inside a full
toolchain without doing a customization build:

``` bash
docker build --target builder -t crypto-payments:dev .
docker run -it crypto-payments:dev bash
```

---

## What is inside the runtime image

At runtime the container contains only:

```
/app/<binary>        # the compiled binary (name comes from Cargo.toml)
/app/wwwroot/        # compiled frontend static files, served by the axum/tower-http router
```

The `frontend/` TypeScript source and `src/` Rust source never reach the runtime
image. They exist only in earlier build stages, which are discarded. This keeps
the final image small and keeps the source out of the distributed artifact —
relevant for a payment processor, since it means a smaller attack surface and no
compiler or toolchain present in the running container.

---

## Customizing the frontend

Two levels, depending on how deep the change goes.

### 1. Logo, styling, and copy — no rebuild

Build a custom `wwwroot` (`npm run build` inside a customized `frontend/` folder,
either locally or in a throwaway container) and bind-mount it over the
container's copy at run time:

``` bash
docker run -v $(pwd)/my-custom-wwwroot:/app/wwwroot:ro \
  -p 8080:8080 --env-file .env \
  ghcr.io/antcdev/crypto-payments:latest
```

Under Compose, uncomment the `volumes:` line beneath the `app:` service instead.

This is the fastest path and needs no Rust or Node toolchain on the operator's
side — just a folder of static files.

### 2. New pages or backend routes — full rebuild

Edit `frontend/` and/or `src/` in a clone and rebuild:

``` bash
docker build -t my-org/crypto-payments .
```

---

## Build pipeline notes

- **cargo-chef** splits "compile dependencies" from "compile application code"
  into separate cached layers. Given the dependency count (`argon2`, `sqlx` with
  several features, `ed25519-dalek`, and others), this is the difference between a
  multi-minute rebuild on every code change and one that takes seconds.

- **`SQLX_OFFLINE=true`** lets the build type-check `sqlx::query!` macros against
  the cached query metadata in `.sqlx/` instead of a live Postgres connection.
  `.sqlx` must be committed to the repository and kept in sync — run
  `cargo sqlx prepare` after changing any query, before building.

- **`libssl3` in the runtime image** is required because `sqlx` is configured with
  the `tls-native-tls` feature, which links OpenSSL rather than the pure-Rust
  `rustls`. Switching `sqlx` to `tls-rustls` allows dropping `libssl3` from the
  runtime stage for a smaller image.

- **Non-root user:** the runtime stage runs as `appuser`. Standard hardening,
  particularly relevant for a service handling private keys and a master key.

- **`HEALTHCHECK`** assumes a `/health` route. Remove the block or repoint it if
  the route differs.

- **`.dockerignore`** excludes screenshots, long-form markdown docs,
  `contract.sol`, and — importantly — `.env`, so secrets are never baked into an
  image layer by accident.

---

## Verifying the deployment

With `docker compose up` running in one terminal:

``` bash
docker ps                          # both containers should show Up
curl http://localhost:8080/        # substitute the configured PORT
docker compose logs -f app         # tail logs without stopping the stack
```

### Confirming `.env` is not in the image

`.env` is listed in `.dockerignore`, so `docker build` never sees it. To verify
rather than assume:

``` bash
docker run --rm crypto-payments-app:latest ls -la /app
# lists only the binary and wwwroot — no .env

docker run --rm crypto-payments-app:latest cat /app/.env
# errors: No such file or directory
```

Configuration reaches the container only at run time, via `env_file: .env` in
`docker-compose.yml` or `docker run --env-file .env`. The image itself is
config-free and byte-identical regardless of who runs it — secrets live in the
container, never in the image.

### Simulating a fresh operator

To confirm the build is genuinely self-contained, build from a clean checkout.
`.env` is gitignored and will not exist there:

``` bash
git clone https://github.com/AntCDev/crypto-payments.git crypto-payments-clone-test
cd crypto-payments-clone-test
cp .env.example .env
# fill in PORT, a throwaway POSTGRES_PASSWORD, and dummy/testnet RPC URLs
docker compose up --build
```

If it builds and boots with placeholder values only, the `Dockerfile` is
self-contained. Networks whose RPC URLs are absent will log
`❌ No valid RPC_URL found` — expected and correct for this test. Delete the clone
directory afterward.

---

## Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `curl` hangs or connection refused | App bound to `127.0.0.1` inside the container | Set `HOST=0.0.0.0` in `.env` |
| App exits immediately on startup | Postgres not ready, or bad credentials | Check `docker compose logs db`; confirm `POSTGRES_*` values |
| Build fails on `sqlx::query!` macros | `.sqlx/` metadata stale or missing | Run `cargo sqlx prepare` locally and commit `.sqlx` |
| `❌ No valid RPC_URL found` | RPC URL unset for that network | Expected if intentional; otherwise set the network's `*_RPC_URLS` |
| Port already allocated | Host port in use | Change `PORT` in `.env` or the `ports:` mapping |

---

## Publishing a prebuilt image (optional)

A minimal GitHub Actions workflow that builds and pushes on tag:

``` yaml
# .github/workflows/docker-publish.yml
name: docker-publish
on:
  push:
    tags: ["v*"]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - uses: docker/build-push-action@v6
        with:
          context: .
          push: true
          tags: ghcr.io/${{ github.repository }}:${{ github.ref_name }}
```

This is not required to run the project — `docker compose up --build` is
sufficient.