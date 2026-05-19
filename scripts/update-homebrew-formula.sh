#!/usr/bin/env bash
set -euo pipefail

formula_path="Formula/keron.rb"
tag=""
checksums=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag) tag="$2"; shift 2 ;;
    --checksums) checksums="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$tag" || -z "$checksums" ]]; then
  echo "usage: $0 --tag vX.Y.Z --checksums path/to/checksums.txt" >&2
  exit 2
fi

if [[ ! -f "$checksums" ]]; then
  echo "checksums file not found: $checksums" >&2
  exit 1
fi

version="${tag#v}"

lookup_checksum() {
  awk -v target="$1" '$2 == target { print $1 }' "$checksums"
}

darwin_arm64_archive="keron_${version}_darwin_arm64.tar.gz"
darwin_amd64_archive="keron_${version}_darwin_amd64.tar.gz"
linux_arm64_archive="keron_${version}_linux_arm64.tar.gz"
linux_amd64_archive="keron_${version}_linux_amd64.tar.gz"

darwin_arm64_sha="$(lookup_checksum "$darwin_arm64_archive")"
darwin_amd64_sha="$(lookup_checksum "$darwin_amd64_archive")"
linux_arm64_sha="$(lookup_checksum "$linux_arm64_archive")"
linux_amd64_sha="$(lookup_checksum "$linux_amd64_archive")"

for pair in \
  "darwin_arm64:$darwin_arm64_sha" \
  "darwin_amd64:$darwin_amd64_sha" \
  "linux_arm64:$linux_arm64_sha" \
  "linux_amd64:$linux_amd64_sha"; do
  name="${pair%%:*}"
  value="${pair#*:}"
  if [[ -z "$value" ]]; then
    echo "missing sha256 for $name in $checksums" >&2
    exit 1
  fi
done

cat >"$formula_path" <<EOF
class Keron < Formula
  desc "User-level dotfile and package manager"
  homepage "https://github.com/icepuma/keron"
  version "${version}"

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
  end

  test do
    assert_match "keron", shell_output("#{bin}/keron --version")
  end
end
EOF
