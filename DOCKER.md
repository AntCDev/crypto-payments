# Running & building rust-crypto with Docker

## The 30-second version

- An **image** is a frozen snapshot built from a `Dockerfile` (your compiled binary + `wwwroot`, in a minimal Linux base).
- A **container** is a running instance of an image.
- `docker-compose.yml` starts multiple containers together (here: the app + Postgres) with one command.

You already have Docker installed, so:

```bash
docker compose up --build
```

This builds the image from `Dockerfile`, starts Postgres, waits for it to be healthy, then starts the app. First build will be slow (compiling all your crates); rebuilds after code-only changes are much faster because of dependency caching (see below).

## One-time setup

1. Put `Dockerfile`, `.dockerignore`, and `docker-compose.yml` in your project root (next to `Cargo.toml`).
2. Make sure `.env` has at least:
   ```
   PORT=8080
   POSTGRES_USER=cryptopay
   POSTGRES_PASSWORD=<something>
   POSTGRES_DB=cryptopay
   ```
   `docker-compose.yml` reads `POSTGRES_*` from `.env` and builds `DATABASE_URL` for you, overriding whatever `DATABASE_URL` is in `.env` so it points at the `db` container instead of `localhost`.
3. `docker compose up --build`
4. App is now on `http://localhost:8080` (or whatever `PORT` you set).

## Why one Dockerfile covers both of your use cases

You were picturing two images: one precompiled, one source-for-building. You don't need to publish two images for that — a **multi-stage Dockerfile** already draws that line for you:

```
frontend-builder  →  compiles frontend/  into wwwroot/
chef / planner    →  figures out your dependency graph
builder           →  compiles src/ into target/release/rust-crypto
runtime           →  copies ONLY the binary + wwwroot into a clean, small base image
```

- **People who just want to run it:** you build the image once (e.g. in CI) and push it to a registry — GitHub Container Registry (`ghcr.io`) is the easy option since your code's already on GitHub. They run `docker pull ghcr.io/you/rust-crypto:latest && docker run ...` and never see Rust or Node at all.
- **People who want to build/customize it themselves:** they clone your repo — which already *is* the full source — and run `docker build .` themselves. The exact same Dockerfile runs the frontend build and the Rust build on their machine and produces their own image with their custom `wwwroot` baked in. There's nothing extra to publish for this case; the Dockerfile is the "source image."

If you also want a convenient always-available toolchain image (for someone who wants to poke around interactively rather than doing a full customization build), the `builder` stage is targetable on its own:

```bash
docker build --target builder -t rust-crypto:dev .
docker run -it rust-crypto:dev bash
```

## Where the binary and frontend actually live

At runtime the container only contains:

```
/app/rust-crypto     # the compiled binary
/app/wwwroot/         # compiled frontend static files, served by your axum/tower-http router
```

The `frontend/` TypeScript source and `src/` Rust source never make it into the runtime image — they're only present in the earlier build stages, which get discarded. This keeps the final image small and means the source code isn't sitting inside the image you distribute (worth knowing since this is a payment processor — smaller attack surface, no compiler/toolchain in the runtime container).

## Letting operators customize the frontend

Two levels, depending on how deep the customization goes:

**1. Swap logo/styling/copy only — no rebuild needed.**
They build their own `wwwroot` (via `npm run build` in their customized `frontend/` folder, on their own machine or in a throwaway container) and bind-mount it over the container's copy at runtime:

```bash
docker run -v $(pwd)/my-custom-wwwroot:/app/wwwroot:ro -p 8080:8080 --env-file .env ghcr.io/you/rust-crypto:latest
```

or in `docker-compose.yml`, uncomment the `volumes:` line under `app:`. This is the fastest path — no Rust or Node toolchain required on their end at all, just a folder of static files.

**2. Deeper changes (new pages, new backend routes, etc.)**
They edit `frontend/` and/or `src/` in their clone and run `docker build -t my-org/rust-crypto .` — full pipeline, their own image, done.

## Notes on the Dockerfile specifics

- **cargo-chef**: splits "compile dependencies" from "compile your code" into separate cached layers. With as many crates as you have (`argon2`, `sqlx` w/ multiple features, `ed25519-dalek`, etc.), this is the difference between a multi-minute rebuild on every code change vs. a few seconds.
- **SQLX_OFFLINE=true**: your `.sqlx/` directory has cached query metadata, so the Docker build doesn't need a live Postgres connection to type-check `sqlx::query!` macros. Make sure `.sqlx` is committed (it's not in your `.gitignore`, so you're already fine) and kept in sync — run `cargo sqlx prepare` locally after changing any query, before building.
- **libssl3 in the runtime image**: needed because your `sqlx` dependency uses the `tls-native-tls` feature, which links OpenSSL rather than the pure-Rust `rustls`. If you ever switch `sqlx` to `tls-rustls`, you can drop `libssl3` from the runtime stage entirely and get an even smaller image.
- **Non-root user**: the runtime stage runs as `appuser`, not root — standard hardening, especially relevant for something handling private keys / master keys.
- **HEALTHCHECK**: assumes a `/health` route. Delete that block if you don't have one, or point it at whatever route you do have.
- **.dockerignore**: excludes your `.jfif` screenshots, the long markdown docs, `contract.sol`, and — importantly — `.env`, so secrets never get baked into an image layer by accident.

## Testing it

With `docker compose up` running in one terminal:

```bash
docker ps                          # confirm both containers are Up
curl http://localhost:3000/        # or whatever PORT you set, from your host
docker compose logs -f app         # tail logs without stopping the stack
```

If `curl` hangs or refuses, it's almost always the app binding to `127.0.0.1`
inside the container instead of `0.0.0.0` — see the `HOST` env var above.
Binding `0.0.0.0` inside a container is fine; Docker's network isolation and
your explicit `ports:` mapping are what actually control real-world exposure,
not the bind address.

## Proving `.env` isn't baked into the image

`.env` is in `.dockerignore`, so `docker build` never even sees it — but it's
easy to prove rather than trust:

```bash
docker run --rm rust-crypto-app:latest ls -la /app
# should list only: rust-crypto, wwwroot  — no .env
docker run --rm rust-crypto-app:latest cat /app/.env
# should error: No such file or directory
```

Config only reaches the container at *run* time, via `env_file: .env` in
`docker-compose.yml` (or `docker run --env-file .env`). The image itself is
config-free and identical no matter who runs it — the secrets live in the
container, not the image.

## Simulating "someone else" running your project

The cleanest way to check a stranger's experience — including confirming
your real `.env` and API keys never leak — is to build from a fresh `git`
checkout, since `.env` is gitignored and simply won't exist there:

```bash
cd ..
git clone C:\Users\PC\Documents\POC\rust-crypto rust-crypto-clone-test
cd rust-crypto-clone-test
copy .env.example .env
# edit .env: fill in PORT, a fake POSTGRES_PASSWORD, dummy/testnet RPC URLs, etc.
docker compose up --build
```

If it builds and boots cleanly with only placeholder values (naturally,
network watchers relying on real RPC URLs will show `❌ No valid RPC_URL
found`, same as your original logs did for Testnet/Bitcoin — that's expected
and fine for this test), you've confirmed the Dockerfile is genuinely
self-contained and nothing from your real `.env` is required or shipped.
Delete the clone folder afterward.

## Publishing the prebuilt image (optional)

A minimal GitHub Actions workflow to build-and-push on tag/release, if useful later:

```yaml
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

This isn't required to get started — `docker compose up --build` locally is enough for now.
