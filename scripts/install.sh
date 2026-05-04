#!/usr/bin/env sh
set -eu

repo="${MUAGENT_REPO:-zhcun/muagent}"
version="${MUAGENT_VERSION:-latest}"
install_dir="${MUAGENT_INSTALL_DIR:-/usr/local/bin}"
dest_name="${MUAGENT_BIN:-muagent}"
uninstall=false

while [ "$#" -gt 0 ]; do
  case "$1" in
    --uninstall)
      uninstall=true
      ;;
    -h | --help)
      echo "usage: install.sh [--uninstall]"
      exit 0
      ;;
    *)
      echo "muagent: unknown option: $1" >&2
      exit 1
      ;;
  esac
  shift
done

dest="${install_dir}/${dest_name}"

if [ "$uninstall" = true ]; then
  if [ ! -e "$dest" ]; then
    echo "muagent: ${dest} is not installed"
    exit 0
  fi
  if [ -w "$install_dir" ]; then
    rm -f "$dest"
  elif command -v sudo >/dev/null 2>&1; then
    sudo rm -f "$dest"
  else
    echo "muagent: ${install_dir} is not writable" >&2
    exit 1
  fi
  echo "muagent: removed ${dest}"
  exit 0
fi

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64 | Darwin-aarch64)
    target="aarch64-apple-darwin"
    ;;
  Darwin-x86_64)
    target="x86_64-apple-darwin"
    ;;
  Linux-x86_64 | Linux-amd64)
    target="x86_64-unknown-linux-musl"
    ;;
  *)
    echo "muagent: unsupported platform: $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "muagent: missing required command: $1" >&2
    exit 1
  fi
}

need tar

tmp="$(mktemp -d 2>/dev/null || mktemp -d -t muagent)"
archive=""
trap 'rm -rf "$tmp"' EXIT INT TERM

strip_v() {
  case "$1" in
    v*)
      printf '%s\n' "${1#v}"
      ;;
    *)
      printf '%s\n' "$1"
      ;;
  esac
}

tag_from_version() {
  case "$version" in
    latest)
      return 1
      ;;
    v*)
      printf '%s\n' "$version"
      ;;
    *)
      printf 'v%s\n' "$version"
      ;;
  esac
}

resolve_latest_with_gh() {
  gh release view --repo "$repo" --json tagName --jq .tagName 2>/dev/null
}

resolve_latest_with_curl() {
  need curl
  release_json="$(curl_get "https://api.github.com/repos/${repo}/releases/latest")"
  tag="$(
    printf '%s\n' "$release_json" |
      sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' |
      head -n 1
  )"
  if [ -z "$tag" ]; then
    echo "muagent: could not resolve latest release tag" >&2
    return 1
  fi
  printf '%s\n' "$tag"
}

resolve_tag() {
  if [ "$version" = "latest" ]; then
    if command -v gh >/dev/null 2>&1; then
      if tag="$(resolve_latest_with_gh)" && [ -n "$tag" ]; then
        printf '%s\n' "$tag"
        return 0
      fi
    fi
    resolve_latest_with_curl
    return
  fi
  tag_from_version
}

curl_get() {
  if [ -n "${GH_TOKEN:-}" ]; then
    curl -fsSL -H "Authorization: Bearer ${GH_TOKEN}" "$@"
  elif [ -n "${GITHUB_TOKEN:-}" ]; then
    curl -fsSL -H "Authorization: Bearer ${GITHUB_TOKEN}" "$@"
  else
    curl -fsSL "$@"
  fi
}

installed_version() {
  if [ ! -x "$dest" ]; then
    return 1
  fi
  "$dest" --version 2>/dev/null |
    sed -n 's/^muagent[[:space:]][[:space:]]*\([^[:space:]]*\).*/\1/p' |
    head -n 1
}

tag="$(resolve_tag)"
target_version="$(strip_v "$tag")"
current_version="$(installed_version || true)"

if [ "$current_version" = "$target_version" ]; then
  echo "muagent: ${dest} is already ${tag}"
  exit 0
fi

asset="muagent-${tag}-${target}.tar.gz"

download_with_gh() {
  gh release download "$tag" --repo "$repo" --pattern "$asset" --dir "$tmp" >/dev/null
  archive="${tmp}/${asset}"
  [ -f "$archive" ]
}

download_with_curl() {
  need curl
  archive="${tmp}/${asset}"
  curl_get -o "$archive" "https://github.com/${repo}/releases/download/${tag}/${asset}"
}

if command -v gh >/dev/null 2>&1; then
  if ! download_with_gh; then
    download_with_curl
  fi
else
  download_with_curl
fi

tar -xzf "$archive" -C "$tmp"
binary="$(find "$tmp" -type f -name muagent | head -n 1)"
if [ -z "$binary" ]; then
  echo "muagent: release archive did not contain a muagent binary" >&2
  exit 1
fi

if mkdir -p "$install_dir" 2>/dev/null && [ -w "$install_dir" ]; then
  install -m 755 "$binary" "$dest"
elif command -v sudo >/dev/null 2>&1; then
  sudo mkdir -p "$install_dir"
  sudo install -m 755 "$binary" "$dest"
else
  echo "muagent: ${install_dir} is not writable; set MUAGENT_INSTALL_DIR" >&2
  exit 1
fi

echo "muagent: installed ${dest}"
"$dest" --version
