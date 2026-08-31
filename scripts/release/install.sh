#!/bin/sh
# Install one exact, checksum-verified Awaken release on supported POSIX hosts.
set -eu

version=${1:-${AWAKEN_VERSION:-}}
if ! printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "usage: install.sh vMAJOR.MINOR.PATCH" >&2
  exit 2
fi

kernel=$(uname -s)
machine=$(uname -m)
case "$kernel:$machine" in
  Linux:x86_64|Linux:amd64)
    target=x86_64-unknown-linux-gnu
    ;;
  Darwin:arm64|Darwin:aarch64)
    target=aarch64-apple-darwin
    ;;
  Darwin:x86_64|Darwin:amd64)
    target=x86_64-apple-darwin
    ;;
  *)
    echo "unsupported platform: $kernel $machine" >&2
    exit 3
    ;;
esac

repository=${AWAKEN_RELEASE_REPOSITORY:-awakenworks/awaken}
base_url=${AWAKEN_RELEASE_BASE_URL:-https://github.com/$repository/releases/download/$version}
archive="awaken-$version-$target.tar.gz"
install_dir=${AWAKEN_INSTALL_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}
temporary=$(mktemp -d "${TMPDIR:-/tmp}/awaken-install.XXXXXX")
staged=
staged_companion=
cleanup() {
  rm -rf -- "$temporary"
  if [ -n "$staged" ]; then
    rm -f -- "$staged"
  fi
  if [ -n "$staged_companion" ]; then
    rm -f -- "$staged_companion"
  fi
}
trap cleanup EXIT HUP INT TERM

download() {
  source_url=$1
  destination=$2
  case "$source_url" in
    https://*)
      curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
        --output "$destination" "$source_url"
      ;;
    *)
      if [ "${AWAKEN_INSTALL_ALLOW_INSECURE:-}" != 1 ]; then
        echo "refusing non-HTTPS release URL: $source_url" >&2
        exit 4
      fi
      curl --fail --silent --show-error --location \
        --output "$destination" "$source_url"
      ;;
  esac
}

download "$base_url/$archive" "$temporary/$archive"
download "$base_url/$archive.sha256" "$temporary/$archive.sha256"
(
  cd "$temporary"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$archive.sha256"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$archive.sha256"
  else
    echo "sha256sum or shasum is required" >&2
    exit 5
  fi
)

package_root=${archive%.tar.gz}
candidate="$temporary/awaken"
candidate_companion="$temporary/awaken-sandbox"
if ! tar -xOzf "$temporary/$archive" "$package_root/awaken" > "$candidate"; then
  echo "release archive does not contain the expected awaken binary" >&2
  exit 6
fi
if ! tar -xOzf "$temporary/$archive" "$package_root/awaken-sandbox" > "$candidate_companion"; then
  echo "release archive does not contain the expected awaken-sandbox companion" >&2
  exit 6
fi
chmod 0755 "$candidate"
chmod 0755 "$candidate_companion"
expected="awaken ${version#v}"
reported=$("$candidate" --version)
if [ "$reported" != "$expected" ]; then
  echo "release binary reports '$reported', expected '$expected'" >&2
  exit 7
fi
if ! "$candidate_companion" hand --check; then
  echo "release companion failed its hand capability check" >&2
  exit 8
fi

mkdir -p -- "$install_dir"
staged="$install_dir/.awaken.$$.tmp"
staged_companion="$install_dir/.awaken-sandbox.$$.tmp"
cp -- "$candidate" "$staged"
cp -- "$candidate_companion" "$staged_companion"
# The companion is installed first; the user-facing `awaken` entrypoint is the
# activation fence and moves only after every payload has been verified/staged.
mv -f -- "$staged_companion" "$install_dir/awaken-sandbox"
staged_companion=
mv -f -- "$staged" "$install_dir/awaken"
staged=

echo "installed $expected and its sandbox companion at $install_dir"
case ":${PATH:-}:" in
  *:"$install_dir":*) ;;
  *) echo "add $install_dir to PATH to run awaken from any directory" ;;
esac
