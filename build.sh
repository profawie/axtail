#!/usr/bin/env sh
# Build the release binary and place it in the repository root as ./axtail
#
#   ./build.sh            Linux: fully static binary (musl). It has no glibc
#                         dependency, so one build runs on any Linux of the same
#                         CPU architecture: Debian, Ubuntu, RHEL, Alpine, old or new.
#   ./build.sh --native   Normal build against this machine's libc (glibc on most
#                         distros). May not run on systems with an older glibc.
set -eu
cd "$(dirname "$0")"

mode=static
case "${1:-}" in
    "") ;;
    --native | --glibc) mode=native ;;
    -h | --help)
        sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "build.sh: unknown option $1 (try --help)" >&2
        exit 2
        ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
    echo "build.sh: cargo was not found. Install Rust from https://rustup.rs" >&2
    echo "          then run: . \"\$HOME/.cargo/env\"" >&2
    exit 1
fi

if ! command -v cc >/dev/null 2>&1; then
    echo "build.sh: no C linker (cc) was found. Rust needs one to link the binary." >&2
    echo "  Debian/Ubuntu:  sudo apt install build-essential" >&2
    echo "  RHEL/Fedora:    sudo dnf install gcc" >&2
    echo "  Alpine:         apk add build-base" >&2
    exit 1
fi

target=""
if [ "$mode" = static ]; then
    if [ "$(uname -s)" != Linux ]; then
        echo "Static musl builds are Linux-only; doing a native build."
    elif ! command -v rustup >/dev/null 2>&1; then
        echo "rustup was not found, so the musl target cannot be added; doing a native build."
        echo "Install Rust with https://rustup.rs to get portable static builds."
    else
        case "$(uname -m)" in
            x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
            aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
            *) echo "No musl target configured for $(uname -m); doing a native build." ;;
        esac
    fi
fi

if [ -n "$target" ]; then
    echo "Building static binary for $target"
    rustup target add "$target"
    cargo build --release --target "$target"
    cp "target/$target/release/axtail" ./axtail
else
    echo "Building native binary"
    cargo build --release
    cp target/release/axtail ./axtail
fi
echo "Built ./axtail"
