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
#
# EXCEPTION (ADR 0009): the `snowflake` extension's ADBC driver
# (libadbc_driver_snowflake, github.com/adbc-drivers/snowflake) is NOT a
# registry extension and does NOT join that fetch-at-first-run deferral — it
# is a native credential-handling library, so it is baked below (the `adbc`
# stage): version-pinned, checksum-verified at image build, never fetched at
# pod start, with SNOWFLAKE_ADBC_DRIVER_PATH pointing the extension at it.

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

# ---- adbc-driver ----
# The Snowflake ADBC driver (ADR 0009 §8): downloaded at IMAGE BUILD, pinned
# by version AND per-arch SHA256 (computed from the go/v1.11.0 release
# artifacts — the release publishes no checksums of its own, so these pins
# are the trust anchor). Bumping ADBC_SNOWFLAKE_VERSION without updating
# both pins fails the build on purpose. TARGETARCH is BuildKit-provided;
# only linux amd64/arm64 images exist upstream.
FROM debian:bookworm-slim AS adbc
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
ARG TARGETARCH
ARG ADBC_SNOWFLAKE_VERSION=1.11.0
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) sha256=514141f69153280f2d3997ce60db79839f28d4c2b75614e7d0923723fd172ee0 ;; \
      arm64) sha256=56dd6a709a67e216337d7aef0db6d4ea533d83581d4fd251237409c32e2a2d3b ;; \
      *) echo "no Snowflake ADBC driver build for TARGETARCH '$TARGETARCH'" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/adbc.tar.gz \
      "https://github.com/adbc-drivers/snowflake/releases/download/go/v${ADBC_SNOWFLAKE_VERSION}/snowflake_linux_${TARGETARCH}_v${ADBC_SNOWFLAKE_VERSION}.tar.gz"; \
    echo "$sha256  /tmp/adbc.tar.gz" | sha256sum -c -; \
    mkdir -p /opt/adbc; \
    tar -xzf /tmp/adbc.tar.gz -C /opt/adbc libadbc_driver_snowflake.so; \
    rm /tmp/adbc.tar.gz

# ---- runtime ----
FROM debian:bookworm-slim
# libstdc++/libgcc: bundled DuckDB links them at runtime.
# ca-certificates: TLS to the object store + the extension registry on first run.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/datamk /usr/local/bin/datamk
# The baked Snowflake ADBC driver (see the adbc stage above). The env var is
# how the `snowflake` extension finds it — no ~/.duckdb lookup in a pod.
COPY --from=adbc /opt/adbc/libadbc_driver_snowflake.so /usr/local/lib/libadbc_driver_snowflake.so
ENV SNOWFLAKE_ADBC_DRIVER_PATH=/usr/local/lib/libadbc_driver_snowflake.so
# In-cluster, pod stderr is the log pipeline; a file log under an emptyDir
# (or worse, no writable volume at all) is an ephemeral-storage eviction
# vector (ADR 0004 §6) for a record nothing else reads. Disable the
# persistent-file-log feature in the deployed image; a local/dev invocation
# of this same binary outside the image still gets it by default.
ENV DATAMK_LOG=off
ENTRYPOINT ["datamk"]
