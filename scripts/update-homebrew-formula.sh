#!/usr/bin/env bash

set -euo pipefail

formula_path="Formula/keron.rb"
tag=""
checksums=""

usage() {
  cat <<'EOF'
Usage:
  scripts/update-homebrew-formula.sh --tag v0.1.0 --checksums dist/SHA256SUMS
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      tag="${2:-}"
      shift 2
      ;;
    --checksums)
      checksums="${2:-}"
      shift 2
      ;;
    --output)
      formula_path="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "$tag" || -z "$checksums" ]]; then
  echo "--tag and --checksums are required" >&2
  usage >&2
  exit 1
fi

if [[ ! -f "$checksums" ]]; then
  echo "checksum file not found: $checksums" >&2
  exit 1
fi

mkdir -p "$(dirname "$formula_path")"

lookup_checksum() {
  local filename="$1"

  awk -v target="$filename" '
    {
      name = $2
      sub(/^.*\//, "", name)
      if (name == target) {
        print $1
        exit
      }
    }
  ' "$checksums"
}

require_checksum() {
  local archive="$1"
  local checksum="$2"

  if [[ -z "$checksum" ]]; then
    echo "missing required archive checksum for $archive in $checksums" >&2
    exit 1
  fi
}

version="${tag#v}"
darwin_arm64_archive="keron-${tag}-aarch64-apple-darwin.tar.gz"
darwin_amd64_archive="keron-${tag}-x86_64-apple-darwin.tar.gz"
linux_arm64_archive="keron-${tag}-aarch64-unknown-linux-musl.tar.gz"
linux_amd64_archive="keron-${tag}-x86_64-unknown-linux-musl.tar.gz"

darwin_arm64_sha="$(lookup_checksum "$darwin_arm64_archive")"
darwin_amd64_sha="$(lookup_checksum "$darwin_amd64_archive")"
linux_arm64_sha="$(lookup_checksum "$linux_arm64_archive")"
linux_amd64_sha="$(lookup_checksum "$linux_amd64_archive")"

require_checksum "$darwin_arm64_archive" "$darwin_arm64_sha"
require_checksum "$darwin_amd64_archive" "$darwin_amd64_sha"
require_checksum "$linux_arm64_archive" "$linux_arm64_sha"
require_checksum "$linux_amd64_archive" "$linux_amd64_sha"

cat >"$formula_path" <<EOF
class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "${version}"
  license "MIT OR Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/${tag}/${darwin_arm64_archive}"
      sha256 "${darwin_arm64_sha}"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/${tag}/${darwin_amd64_archive}"
      sha256 "${darwin_amd64_sha}"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/icepuma/keron/releases/download/${tag}/${linux_arm64_archive}"
      sha256 "${linux_arm64_sha}"
    end

    on_intel do
      url "https://github.com/icepuma/keron/releases/download/${tag}/${linux_amd64_archive}"
      sha256 "${linux_amd64_sha}"
    end
  end

  def install
    bin.install "keron"
    doc.install "README.md"
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
EOF
