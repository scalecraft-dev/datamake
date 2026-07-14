# Cell-agnostic datamk base image (ADR 0001 §5): the `datamk` binary + runtime.
# Cell content (cell.yaml, sql/, the release pin) is delivered at deploy time —
# users do NOT build a per-cell image. The image tag is meant to track the binary
# version, so a given `datamk` deploys the matching base image.
#
# Build layout: cargo-chef splits dependency compilation (including the bundled
# DuckDB C++ build, by far the dominant cost) into a layer keyed on the recipe
# (≈ Cargo.lock), so it caches across releases — CI persists it in the GHCR
# registry (`:buildcache`, see .github/workflows/base-image.yml) and a release
# that doesn't change dependencies only compiles the datamk crate itself.
#
# NOTE: DuckDB extensions (ducklake, httpfs, json, and connector extensions such
# as `bigquery` from the community registry — ADR 0003) are fetched at first run
# (engine INSTALLs them), so a running container needs network egress to the
# extension registries. Baking the extensions into the image — and a no-INSTALL
# runtime path for egress-less environments — is a deliberate follow-up (ADR 0001
# §5 / ADR 0003 §4, deferred); revisit when such an environment is encountered.

# ---- chef ----
# buildpack-deps base (via the official rust image) carries the C/C++ toolchain
# the bundled DuckDB build needs.
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

# ---- planner ----
# Distills the workspace into a dependency recipe. Source edits change the
# recipe only if they change dependency structure, keeping the cook layer warm.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder ----
FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
# The expensive, cacheable layer: compiles every dependency (bundled DuckDB
# included) with no datamk source present.
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
# Only the datamk crate compiles here on a warm cache.
RUN cargo build --release --locked --bin datamk

# ---- runtime ----
FROM debian:bookworm-slim
# libstdc++/libgcc: bundled DuckDB links them at runtime.
# ca-certificates: TLS to the object store + the extension registry on first run.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/datamk /usr/local/bin/datamk
# In-cluster, pod stderr is the log pipeline; a file log under an emptyDir
# (or worse, no writable volume at all) is an ephemeral-storage eviction
# vector (ADR 0004 §6) for a record nothing else reads. Disable the
# persistent-file-log feature in the deployed image; a local/dev invocation
# of this same binary outside the image still gets it by default.
ENV DATAMK_LOG=off
ENTRYPOINT ["datamk"]
