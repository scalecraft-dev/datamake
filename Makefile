# Developer convenience targets. CI runs the same checks (see .github/workflows/ci.yml).
.PHONY: all fmt fmt-check lint test check release e2e e2e-up e2e-deploy e2e-validate e2e-down

# Run the full local gate: format, lint, test.
all: fmt lint test

# Apply formatting.
fmt:
	cargo fmt --all

# Verify formatting without changing files (what CI runs).
fmt-check:
	cargo fmt --all -- --check

# Lint with clippy, failing on any warning.
lint:
	cargo clippy --all-targets --all-features -- -D warnings

# Run the test suite.
test:
	cargo test

# Mirror CI exactly: fmt check + clippy + tests.
check: fmt-check lint test

# Cut a release: stamps Cargo.toml to VERSION, runs the full gate, commits,
# tags, and pushes. CI (.github/workflows/base-image.yml) then builds and
# publishes ghcr.io/scalecraft-dev/datamk:<version>. Usage:
#   make release VERSION=v0.1.0
release:
	./scripts/release.sh $(VERSION)

# ---------------------------------------------------------------------------
# ADR 0002 (Kubernetes deploy target) end-to-end harness against a real `kind`
# cluster. LOCAL-ONLY: requires docker/kind/kubectl on PATH and is NOT part of
# `make check` or CI (.github/workflows/ never touches it) -- it mutates local
# Docker/kind state and builds a full release image. See
# test/integrations/kind_e2e/README.md.

# Full run: cluster up -> image build -> infra -> deploy -> validate. Leaves
# the cluster running on success; tear it down with `make e2e-down`.
e2e:
	./test/integrations/kind_e2e/run.sh all

# Just the kind cluster + namespace (useful when iterating on later phases).
e2e-up:
	./test/integrations/kind_e2e/run.sh up

# Re-run `datamk deploy` against the already-up cluster (e.g. after editing
# the deploy overlay or the render/apply code and rebuilding).
e2e-deploy:
	./test/integrations/kind_e2e/run.sh deploy

# Re-run just the assertions against an already-deployed cell.
e2e-validate:
	./test/integrations/kind_e2e/run.sh validate

# Delete the kind cluster.
e2e-down:
	./test/integrations/kind_e2e/run.sh down
