#!/bin/sh
set -eu

BIN=latte-lens
REPOSITORY=${LATTE_LENS_REPOSITORY:-latte-co/latte-lens}
API_BASE=${LATTE_LENS_API_URL:-https://api.github.com/repos/$REPOSITORY}
DOWNLOAD_BASE=${LATTE_LENS_DOWNLOAD_URL:-https://github.com/$REPOSITORY/releases/download}
INSTALL_DIR=${LATTE_LENS_INSTALL_DIR:-$HOME/.local/bin}
REQUESTED_VERSION=${LATTE_LENS_VERSION:-}
TMP=
DESTINATION_TMP=

log() {
  printf ' \033[32m>\033[0m %s\n' "$1"
}

warn() {
  printf ' \033[33m!\033[0m %s\n' "$1"
}

err() {
  printf ' \033[31m✗\033[0m %s\n' "$1" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || err "requires '$1'; install it first"
}

cleanup() {
  if [ -n "$DESTINATION_TMP" ]; then
    rm -f "$DESTINATION_TMP"
  fi
  if [ -n "$TMP" ]; then
    rm -rf "$TMP"
  fi
}

api_get() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl -fsSL --retry 3 --connect-timeout 10 --max-time 30 \
      -H 'Accept: application/vnd.github+json' \
      -H "Authorization: Bearer $GITHUB_TOKEN" \
      "$1"
  else
    curl -fsSL --retry 3 --connect-timeout 10 --max-time 30 \
      -H 'Accept: application/vnd.github+json' \
      "$1"
  fi
}

download() {
  curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 "$1" -o "$2"
}

manifest_tag() {
  printf '%s\n' "$1" | awk -F '"' '
    {
      for (i = 1; i < NF; i++) {
        if ($i == "tag_name") {
          print $(i + 2)
          exit
        }
      }
    }
  '
}

detect_target() {
  detected_os=$(uname -s)
  detected_arch=$(uname -m)

  case "$detected_os" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) err "unsupported operating system: $detected_os" ;;
  esac

  case "$detected_arch" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) err "unsupported architecture: $detected_arch" ;;
  esac

  case "$os/$arch" in
    linux/x86_64) target=x86_64-unknown-linux-gnu ;;
    linux/aarch64) target=aarch64-unknown-linux-gnu ;;
    macos/x86_64) target=x86_64-apple-darwin ;;
    macos/aarch64) target=aarch64-apple-darwin ;;
    *) err "no release package is available for $os/$arch" ;;
  esac

  log "detected $os/$arch"
}

resolve_release() {
  API_BASE=${API_BASE%/}
  DOWNLOAD_BASE=${DOWNLOAD_BASE%/}

  if [ -n "$REQUESTED_VERSION" ]; then
    case "$REQUESTED_VERSION" in
      v*) requested_tag=$REQUESTED_VERSION ;;
      *) requested_tag=v$REQUESTED_VERSION ;;
    esac
    manifest=$(api_get "$API_BASE/releases/tags/$requested_tag") \
      || err "release '$requested_tag' was not found"
  elif manifest=$(api_get "$API_BASE/releases/latest" 2>/dev/null); then
    :
  else
    warn "no stable release found; falling back to the latest preview"
    manifest=$(api_get "$API_BASE/releases?per_page=1") \
      || err "could not fetch the latest release from GitHub"
  fi

  tag=$(manifest_tag "$manifest")
  [ -n "$tag" ] || err "release metadata did not include a tag"
  case "$tag" in
    *[!A-Za-z0-9._-]*) err "release metadata included an unsafe tag: $tag" ;;
  esac

  case "$tag" in
    v*) version=${tag#v} ;;
    *) version=$tag ;;
  esac
  case "$version" in
    *-*) warn "installing preview release $tag" ;;
  esac
}

verify_checksum() {
  expected=$(awk 'NR == 1 { print $1; exit }' "$2")
  [ "${#expected}" -eq 64 ] || err "invalid checksum file for $1"
  case "$expected" in
    *[!0-9A-Fa-f]*) err "invalid checksum file for $1" ;;
  esac

  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$1" | awk '{ print $1 }')
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$1" | awk '{ print $1 }')
  else
    err "requires 'sha256sum' or 'shasum' to verify the download"
  fi

  [ "$actual" = "$expected" ] || err "checksum verification failed for $1"
}

install_release() {
  package="latte-lens-$version-$target"
  archive="$package.tar.gz"
  archive_url="$DOWNLOAD_BASE/$tag/$archive"
  checksum_url="$archive_url.sha256"

  TMP=$(mktemp -d)
  trap cleanup 0
  trap 'exit 1' 1 2 3 15

  log "downloading $tag"
  download "$archive_url" "$TMP/$archive" \
    || err "could not download $archive_url"
  download "$checksum_url" "$TMP/$archive.sha256" \
    || err "could not download $checksum_url"
  verify_checksum "$TMP/$archive" "$TMP/$archive.sha256"

  tar -xzf "$TMP/$archive" -C "$TMP"
  source_binary="$TMP/$package/$BIN"
  [ -f "$source_binary" ] || err "release archive did not contain $BIN"
  chmod +x "$source_binary"
  reported_version=$("$source_binary" --version 2>/dev/null) \
    || err "downloaded binary could not run on this system"
  case "$reported_version" in
    *"$version"*) ;;
    *) err "downloaded binary reported an unexpected version: $reported_version" ;;
  esac

  mkdir -p "$INSTALL_DIR"
  destination="$INSTALL_DIR/$BIN"
  DESTINATION_TMP="$INSTALL_DIR/.$BIN.tmp.$$"
  cp "$source_binary" "$DESTINATION_TMP"
  chmod 755 "$DESTINATION_TMP"
  mv -f "$DESTINATION_TMP" "$destination"
  DESTINATION_TMP=

  log "installed $reported_version to $destination"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      warn "$INSTALL_DIR is not in your PATH"
      printf '\n  export PATH="%s:$PATH"\n\n' "$INSTALL_DIR"
      ;;
  esac
}

main() {
  need curl
  need awk
  need tar
  need uname
  need mktemp
  need cp
  need chmod
  need mkdir
  need mv
  detect_target
  resolve_release
  install_release
}

main "$@"
