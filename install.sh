#!/usr/bin/env sh
# Install axtail on this machine.
#
# One line, straight from GitHub (no Rust needed on the server):
#   curl -fsSL https://raw.githubusercontent.com/profawie/axtail/main/install.sh | sudo sh
#   curl -fsSL https://raw.githubusercontent.com/profawie/axtail/main/install.sh | sh
#
# Or from a copy of this script:
#   ./install.sh                 Install to /usr/local/bin when run as root,
#                                otherwise to ~/.local/bin
#   sudo ./install.sh            Install system-wide for all users
#   ./install.sh --dir <DIR>     Install into a specific directory
#   ./install.sh --version <TAG> Download a specific release, e.g. v0.1.0
#   ./install.sh --download      Always download, even if ./axtail exists
#   ./install.sh --uninstall     Remove axtail (add --dir <DIR> if you used one)
#
# Where the binary comes from, in order: an ./axtail file next to this script,
# a local build when run inside a clone with Rust installed, otherwise the
# static binary from the GitHub release (verified against its SHA256SUMS).
# Options also work when piped:  curl ... | sh -s -- --dir /opt/bin
set -eu

# AXTAIL_REPO_URL can point at a mirror or fork.
repo="${AXTAIL_REPO_URL:-https://github.com/profawie/axtail}"

# When piped into sh there is no script file, so nothing local can be used.
here=""
case "$0" in
    *install.sh) [ -f "$0" ] && here="$(cd "$(dirname "$0")" && pwd)" ;;
esac

dir=""
uninstall=0
download=0
tag=""
while [ $# -gt 0 ]; do
    case "$1" in
        --dir)
            [ $# -ge 2 ] || { echo "install.sh: --dir needs a directory" >&2; exit 2; }
            dir="$2"
            shift
            ;;
        --version)
            [ $# -ge 2 ] || { echo "install.sh: --version needs a tag such as v0.1.0" >&2; exit 2; }
            tag="$2"
            download=1
            shift
            ;;
        --download) download=1 ;;
        --uninstall) uninstall=1 ;;
        -h | --help)
            if [ -n "$here" ]; then
                sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            else
                echo "Options: --dir <DIR>, --version <TAG>, --download, --uninstall"
            fi
            exit 0
            ;;
        *)
            echo "install.sh: unknown option $1 (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

if [ -z "$dir" ]; then
    if [ "$(id -u)" -eq 0 ]; then
        dir=/usr/local/bin
    else
        dir="$HOME/.local/bin"
    fi
fi
dest="$dir/axtail"

if [ "$uninstall" -eq 1 ]; then
    if [ -e "$dest" ]; then
        rm -f "$dest"
        echo "Removed $dest"
    else
        echo "axtail is not installed in $dir"
        [ "$(id -u)" -eq 0 ] || echo "If it was installed system-wide, run: sudo ./install.sh --uninstall"
    fi
    exit 0
fi

# Downloads the static release binary for this machine into $1.
fetch_release() {
    if [ "$(uname -s)" != Linux ]; then
        echo "install.sh: release binaries are only published for Linux and Windows." >&2
        echo "  On $(uname -s), build from source with ./build.sh" >&2
        exit 1
    fi
    case "$(uname -m)" in
        x86_64 | amd64) asset=axtail-linux-x86_64 ;;
        aarch64 | arm64) asset=axtail-linux-aarch64 ;;
        *)
            echo "install.sh: no release binary for CPU architecture $(uname -m)." >&2
            echo "  Build from source with ./build.sh" >&2
            exit 1
            ;;
    esac
    if [ -n "$tag" ]; then
        base="$repo/releases/download/$tag"
    else
        base="$repo/releases/latest/download"
    fi
    if command -v curl >/dev/null 2>&1; then
        get() { curl -fsSL --retry 3 -o "$2" "$1"; }
    elif command -v wget >/dev/null 2>&1; then
        get() { wget -q -O "$2" "$1"; }
    else
        echo "install.sh: curl or wget is required to download axtail." >&2
        exit 1
    fi
    echo "Downloading $base/$asset"
    if ! get "$base/$asset" "$1/axtail"; then
        echo "install.sh: download failed. Check the network, or that the release exists:" >&2
        echo "  $repo/releases" >&2
        exit 1
    fi
    if command -v sha256sum >/dev/null 2>&1 && get "$base/SHA256SUMS" "$1/SHA256SUMS"; then
        want="$(sed -n "s/^\([0-9a-f]\{64\}\)  *\**$asset\$/\1/p" "$1/SHA256SUMS")"
        have="$(sha256sum "$1/axtail" | cut -d' ' -f1)"
        if [ -z "$want" ] || [ "$want" != "$have" ]; then
            echo "install.sh: checksum mismatch for $asset; refusing to install." >&2
            exit 1
        fi
        echo "Checksum verified"
    else
        echo "Warning: could not verify the download (sha256sum or SHA256SUMS unavailable)." >&2
    fi
}

if ! mkdir -p "$dir" 2>/dev/null || [ ! -w "$dir" ]; then
    echo "install.sh: cannot write to $dir. Run with sudo, or choose another place with --dir." >&2
    exit 1
fi

work=""
trap '[ -z "$work" ] || rm -rf "$work"' EXIT

bin=""
if [ "$download" -eq 0 ] && [ -n "$here" ]; then
    if [ -f "$here/axtail" ]; then
        bin="$here/axtail"
    elif command -v cargo >/dev/null 2>&1 && [ -f "$here/build.sh" ]; then
        echo "No ./axtail binary yet; building it first."
        sh "$here/build.sh"
        bin="$here/axtail"
    fi
fi
if [ -z "$bin" ]; then
    # Download next to the destination: /tmp is mounted noexec on some servers.
    work="$(mktemp -d "$dir/.axtail-install.XXXXXX")"
    fetch_release "$work"
    bin="$work/axtail"
fi

# Catch a binary built for another CPU architecture before installing it.
chmod +x "$bin" 2>/dev/null || true
if ! version="$("$bin" --version 2>/dev/null)"; then
    echo "install.sh: $bin does not run on this machine ($(uname -s) $(uname -m))." >&2
    echo "  It was probably built for a different CPU architecture or operating system." >&2
    exit 1
fi

# Copy to a temporary name and rename, so a running axtail is never half-replaced.
tmp="$dest.tmp.$$"
cp "$bin" "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "$dest"
echo "Installed $version to $dest"

case ":$PATH:" in
    *":$dir:"*) echo "Run it with: axtail" ;;
    *)
        echo "$dir is not in your PATH. Add this line to ~/.profile or ~/.bashrc:"
        echo "  export PATH=\"$dir:\$PATH\""
        echo "Or run it directly: $dest"
        ;;
esac
