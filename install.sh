#!/bin/sh
# datamk installer — fetches a prebuilt release binary from GitHub Releases,
# verifies its checksum, and installs it. Designed for:
#
#   curl -fsSL https://raw.githubusercontent.com/scalecraft-dev/datamake/main/install.sh | sh
#
# Configuration (env vars — a piped script can't take flags honestly):
#   DATAMK_VERSION      release tag to install (default: latest), e.g. v0.0.4
#   DATAMK_INSTALL_DIR  install directory (default: ~/.local/bin)
#
# POSIX sh on purpose: this runs before the user has installed anything.
set -eu

REPO="scalecraft-dev/datamake"

say() { printf '%s\n' "$*"; }
die() { printf 'datamk: %s\n' "$*" >&2; exit 1; }

# --- platform ---------------------------------------------------------------
# Prebuilt targets exist for Apple Silicon macOS and glibc 2.28+ Linux.
# Everything else gets a named, actionable refusal — never a tarball that
# fails later with a cryptic loader error.
os="$(uname -s)"
arch="$(uname -m)"

cargo_fallback="cargo install --git https://github.com/${REPO} datamk"

case "$os" in
  Darwin)
    case "$arch" in
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      x86_64) die "no prebuilt binary for Intel Macs. Build from source instead:
  ${cargo_fallback}" ;;
      *) die "unsupported macOS architecture: ${arch}" ;;
    esac
    ;;
  Linux)
    # musl systems (Alpine) can't run the glibc binaries.
    if ldd --version 2>&1 | grep -qi musl; then
      die "no musl build yet. On Alpine, build from source (${cargo_fallback}) or use a glibc image."
    fi
    case "$arch" in
      x86_64) target="x86_64-unknown-linux-gnu" ;;
      arm64 | aarch64) target="aarch64-unknown-linux-gnu" ;;
      *) die "unsupported Linux architecture: ${arch}" ;;
    esac
    ;;
  MINGW* | MSYS* | CYGWIN* | Windows_NT)
    die "datamk runs on Windows via WSL2. Inside a WSL2 shell, re-run:
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | sh"
    ;;
  *)
    die "unsupported platform: ${os}/${arch}"
    ;;
esac

# --- version ----------------------------------------------------------------
version="${DATAMK_VERSION:-latest}"
if [ "$version" = "latest" ]; then
  # The /releases/latest redirect names the tag without needing jq or auth.
  version="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
    "https://github.com/${REPO}/releases/latest" | sed 's|.*/tag/||')"
  case "$version" in
    v*) ;;
    *) die "could not resolve the latest release (got '${version}'). No releases yet, or GitHub is unreachable." ;;
  esac
fi

# --- download + verify ------------------------------------------------------
name="datamk-${version}-${target}"
base_url="https://github.com/${REPO}/releases/download/${version}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading datamk ${version} (${target})..."
curl -fsSL -o "${tmp}/${name}.tar.gz" "${base_url}/${name}.tar.gz" \
  || die "download failed: ${base_url}/${name}.tar.gz
Does release ${version} exist and include ${target}? See https://github.com/${REPO}/releases"
curl -fsSL -o "${tmp}/sha256sums.txt" "${base_url}/sha256sums.txt" \
  || die "download failed: ${base_url}/sha256sums.txt"

# Checksum verification is not optional: curl|sh is already a trust ask.
if command -v sha256sum >/dev/null 2>&1; then
  sum_tool="sha256sum"
else
  sum_tool="shasum -a 256" # macOS ships shasum, not sha256sum
fi
(
  cd "$tmp"
  grep " ${name}.tar.gz\$" sha256sums.txt | $sum_tool -c - >/dev/null 2>&1
) || die "checksum verification FAILED for ${name}.tar.gz — refusing to install."

# --- install ----------------------------------------------------------------
install_dir="${DATAMK_INSTALL_DIR:-${HOME}/.local/bin}"
mkdir -p "$install_dir"
tar xzf "${tmp}/${name}.tar.gz" -C "$tmp"
install -m 0755 "${tmp}/${name}/datamk" "${install_dir}/datamk"

say "installed datamk ${version} -> ${install_dir}/datamk"

# PATH check: print the fix, never edit the user's shell config.
case ":${PATH}:" in
  *:"${install_dir}":*) ;;
  *)
    say ""
    say "${install_dir} is not on your PATH. Add it:"
    say "  export PATH=\"${install_dir}:\$PATH\""
    say "(put that in your ~/.zshrc or ~/.bashrc)"
    ;;
esac

say ""
say "Next:"
say "  datamk --help"
say "  datamk init my-cell"
