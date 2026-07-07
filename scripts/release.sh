#!/usr/bin/env bash
# Cut a release: stamp Cargo.toml to the given version, run the full local
# gate, commit, tag, and push. CI's base-image workflow
# (.github/workflows/base-image.yml, `on: push: tags: v*`) then builds and
# publishes ghcr.io/scalecraft-dev/datamk:vX.Y.Z (+ :latest) from the tag.
#
# The version stamp is load-bearing, not cosmetic: the Kubernetes target's
# default image is ghcr.io/scalecraft-dev/datamk:<CARGO_PKG_VERSION>
# (src/deploy/targets/kubernetes/schema.rs::image_ref, ADR 0001 §5) — a given
# datamk binary deploys its *matching* base image only if the binary built
# from this tag carries this version. The workflow enforces the same
# consistency from the other side.
#
# Usage (via the Makefile): make release VERSION=v0.1.0
set -euo pipefail

die() { echo "release: $*" >&2; exit 1; }

# Non-interactive shells (CI, make from an IDE) often lack the rustup PATH.
command -v cargo >/dev/null 2>&1 || export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH (or in ~/.cargo/bin)"

VERSION="${1:-}"
[[ "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || die "usage: make release VERSION=vX.Y.Z (got: '${VERSION:-<empty>}')"
SEMVER="${VERSION#v}"

# From the repo root, whatever directory make was invoked in.
cd "$(git rev-parse --show-toplevel)"

# Releases cut from a clean, up-to-date main only.
branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "main" ] || die "releases are cut from main (currently on '$branch')"
git diff --quiet && git diff --cached --quiet \
  || die "working tree is not clean; commit or stash first"
git fetch origin main --tags --quiet
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] \
  || die "main is not in sync with origin/main; pull/push first"
if git rev-parse -q --verify "refs/tags/$VERSION" >/dev/null; then
  die "tag $VERSION already exists"
fi

echo "==> stamping Cargo.toml to $SEMVER"
perl -pi -e "s/^version = \".*\"\$/version = \"$SEMVER\"/ if !\$done && (\$done = /^version = /)" Cargo.toml
grep -q "^version = \"$SEMVER\"$" Cargo.toml || die "failed to stamp Cargo.toml"
# Sync the lockfile's own entry for datamk without touching dependencies.
cargo update --workspace --quiet

echo "==> running the release gate (fmt-check + clippy + tests)"
make check

echo "==> committing, tagging, and pushing $VERSION"
git commit -am "Release $VERSION"
git tag -a "$VERSION" -m "datamk $VERSION"
git push origin main "$VERSION"

cat <<EOF

Release $VERSION pushed. CI is now building and publishing:
  ghcr.io/scalecraft-dev/datamk:$VERSION
  ghcr.io/scalecraft-dev/datamk:latest
  GitHub Release $VERSION with host binaries (macOS arm64, Linux x86_64/arm64)
Watch it: gh run watch --repo scalecraft-dev/datamk \$(gh run list --repo scalecraft-dev/datamk --workflow base-image.yml --limit 1 --json databaseId --jq '.[0].databaseId')
EOF
