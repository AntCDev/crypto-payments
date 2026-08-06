# syntax=docker/dockerfile:1.7
#
# Multi-stage build for rust-crypto.
#
# Usage:
#   docker build -t rust-crypto:latest .
#   docker build --target builder -t rust-crypto:dev .   # toolchain-only image, for iterating locally
#
# The same file serves both audiences: people who just want the compiled
# app get the small final "runtime" stage; people who want to build from
# source (e.g. to customize the frontend) run this exact file themselves.

########################################
# 1. Frontend build stage
########################################
FROM node:20-alpine AS frontend-builder
WORKDIR /app/frontend

COPY frontend/package.json frontend/package-lock.json* frontend/pnpm-lock.yaml* frontend/yarn.lock* ./
RUN if [ -f package-lock.json ]; then npm ci; \
    elif [ -f pnpm-lock.yaml ]; then corepack enable && pnpm install --frozen-lockfile; \
    elif [ -f yarn.lock ]; then corepack enable && yarn install --frozen-lockfile; \
    else npm install; fi

COPY frontend/ .
RUN npm run build

########################################
# 2. Rust dependency cache stage (cargo-chef)
########################################
FROM rust:1-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

########################################
# 3. Rust build stage
########################################
FROM chef AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY .sqlx ./.sqlx
ENV SQLX_OFFLINE=true

RUN cargo build --release --locked
RUN strip target/release/rust-crypto

########################################
# 4. Runtime stage — this is the small image people actually pull/run
########################################
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        wget \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 1001 --create-home appuser

WORKDIR /app
COPY --from=builder /app/target/release/rust-crypto ./rust-crypto
COPY --from=frontend-builder /app/wwwroot ./wwwroot

RUN chown -R appuser:appuser /app
USER appuser

ENV RUST_LOG=info
EXPOSE 8080

ENTRYPOINT ["./rust-crypto"]
