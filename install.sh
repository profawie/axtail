#!/usr/bin/env sh
# Install axtail on this machine.
#
#   ./install.sh                 Install to /usr/local/bin when run as root,
#                                otherwise to ~/.local/bin
#   sudo ./install.sh            Install system-wide for all users
#   ./install.sh --dir <DIR>     Install into a specific directory
#   ./install.sh --uninstall     Remove axtail (add --dir <DIR> if you used one)
#
# The script installs the ./axtail binary that sits next to it. On a server
# without Rust, copy just two files (axtail and install.sh) and run it there.
# If ./axtail is missing and Rust is available, it is built first.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"

dir=""
uninstall=0
while [ $# -gt 0 ]; do
    case "$1" in
        --dir)
            [ $# -ge 2 ] || { echo "install.sh: --dir needs a directory" >&2; exit 2; }
            dir="$2"
            shift
            ;;
        --uninstall) uninstall=1 ;;
        -h | --help)
            sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
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

bin="$here/axtail"
if [ ! -f "$bin" ]; then
    if command -v cargo >/dev/null 2>&1 && [ -f "$here/build.sh" ]; then
        echo "No ./axtail binary yet; building it first."
        sh "$here/build.sh"
    else
        echo "install.sh: $bin was not found." >&2
        echo "  Build it on a machine with Rust (./build.sh) and copy the axtail file" >&2
        echo "  next to this script, then run ./install.sh again." >&2
        exit 1
    fi
fi

# Catch a binary built for another CPU architecture before installing it.
chmod +x "$bin" 2>/dev/null || true
if ! version="$("$bin" --version 2>/dev/null)"; then
    echo "install.sh: $bin does not run on this machine ($(uname -s) $(uname -m))." >&2
    echo "  It was probably built for a different CPU architecture or operating system." >&2
    exit 1
fi

if ! mkdir -p "$dir" 2>/dev/null || [ ! -w "$dir" ]; then
    echo "install.sh: cannot write to $dir. Run with sudo, or choose another place with --dir." >&2
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
