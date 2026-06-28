# Developer convenience targets. CI runs the same checks (see .github/workflows/ci.yml).
.PHONY: all fmt fmt-check lint test check

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
