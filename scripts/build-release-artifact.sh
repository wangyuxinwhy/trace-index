#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/build-release-artifact.sh [--target TARGET] --output-root DIR [--version VERSION]

Build trace-index with Cargo, then package it with package-release.sh. TARGET
defaults to rustc's host triple. VERSION, when supplied, is checked against the
Cargo package version; release CI uses that check to reject a tag/version
mismatch. Native binaries also execute `--version` before packaging.
EOF
}

fail() {
    echo "build-release-artifact: $*" >&2
    exit 1
}

TARGET=""
OUTPUT_ROOT=""
VERSION=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --target)
            [ "$#" -ge 2 ] || fail "--target requires a value"
            TARGET="$2"
            shift 2
            ;;
        --output-root)
            [ "$#" -ge 2 ] || fail "--output-root requires a value"
            OUTPUT_ROOT="$2"
            shift 2
            ;;
        --version)
            [ "$#" -ge 2 ] || fail "--version requires a value"
            VERSION="${2#v}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

[ -n "$OUTPUT_ROOT" ] || fail "--output-root is required"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [ -z "$TARGET" ]; then
    TARGET="$(rustc -vV | sed -n 's/^host: //p')"
fi
[ -n "$TARGET" ] || fail "could not determine the Rust host target"

HOST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$HOST_TARGET" ] || fail "could not determine the Rust host target"
PACKAGE_ID="$(cargo pkgid)"
case "$PACKAGE_ID" in
    *'#'*) PACKAGE_VERSION="${PACKAGE_ID##*#}" ;;
    *@*) PACKAGE_VERSION="${PACKAGE_ID##*@}" ;;
    *) fail "could not read a version from cargo pkgid: $PACKAGE_ID" ;;
esac
if [ -z "$VERSION" ]; then
    VERSION="$PACKAGE_VERSION"
fi
[ "$VERSION" = "$PACKAGE_VERSION" ] ||
    fail "requested version $VERSION does not match Cargo package version $PACKAGE_VERSION"

echo "==> Building trace-index for $TARGET" >&2
cargo build --release --locked --target "$TARGET" --bin trace-index

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$REPO_ROOT/$TARGET_DIR" ;;
esac

BINARY="$TARGET_DIR/$TARGET/release/trace-index"
ARGS=(
    --binary "$BINARY"
    --target "$TARGET"
    --output-root "$OUTPUT_ROOT"
)
ARGS+=(--version "$VERSION")
if [ "$TARGET" != "$HOST_TARGET" ]; then
    echo "==> Cross target $TARGET cannot be executed on $HOST_TARGET; deferring the executable version check to installation" >&2
    ARGS+=(--skip-binary-version-check)
fi

"$REPO_ROOT/scripts/package-release.sh" "${ARGS[@]}"
