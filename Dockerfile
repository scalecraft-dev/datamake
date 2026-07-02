# Cell-agnostic datamk base image (ADR 0001 §5): the `datamk` binary + runtime.
# Cell content (cell.yaml, sql/, the release pin) is delivered at deploy time —
# users do NOT build a per-cell image. The image tag is meant to track the binary
# version, so a given `datamk` deploys the matching base image.
#
# NOTE: DuckDB extensions (ducklake, httpfs, json, and connector extensions such
# as `bigquery` from the community registry — ADR 0003) are fetched at first run
# (engine INSTALLs them), so a running container needs network egress to the
# extension registries. Baking the extensions into the image — and a no-INSTALL
# runtime path for egress-less environments — is a deliberate follow-up (ADR 0001
# §5 / ADR 0003 §4, deferred); revisit when such an environment is encountered.

# ---- builder ----
# buildpack-deps base (via the official rust image) carries the C/C++ toolchain
# the bundled DuckDB build needs.
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
# Release build with default features (includes the kubernetes deploy target).
RUN cargo build --release --locked --bin datamk

# ---- runtime ----
FROM debian:bookworm-slim
# libstdc++/libgcc: bundled DuckDB links them at runtime.
# ca-certificates: TLS to the object store + the extension registry on first run.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/datamk /usr/local/bin/datamk
ENTRYPOINT ["datamk"]
