#!/bin/sh
# sekio installer.
#
#   curl -fsSL https://raw.githubusercontent.com/hairbui76/sekio/main/install.sh | sh
#
# Downloads the release archive for this machine, verifies its SHA-256 against
# the .sha256 published beside it, and installs the binaries into
# $SEKIO_PREFIX (default ~/.local/bin).
#
# POSIX sh on purpose: this runs on whatever /bin/sh a distro ships, and it is
# read from a pipe, so it never reads stdin.

set -eu

REPO="hairbui76/sekio"
BASE_URL="https://github.com/${REPO}/releases/download"
API_URL="https://api.github.com/repos/${REPO}/releases/latest"

# The archive ships README.md and LICENSE-MIT too; only these get installed.
BINS="sekio sekio-tui sekio-gui"

TMPDIR_SEKIO=""

# ---------------------------------------------------------------- output ----

if [ -t 2 ]; then
    C_RED=$(printf '\033[31m')
    C_YELLOW=$(printf '\033[33m')
    C_GREEN=$(printf '\033[32m')
    C_BOLD=$(printf '\033[1m')
    C_OFF=$(printf '\033[0m')
else
    C_RED='' C_YELLOW='' C_GREEN='' C_BOLD='' C_OFF=''
fi

say() { printf '%s\n' "$*" >&2; }
info() { printf '%s\n' "${C_BOLD}sekio${C_OFF}: $*" >&2; }
warn() { printf '%s\n' "${C_YELLOW}warning${C_OFF}: $*" >&2; }
ok() { printf '%s\n' "${C_GREEN}$*${C_OFF}" >&2; }
err() {
    printf '%s\n' "${C_RED}error${C_OFF}: $*" >&2
    exit 1
}

cleanup() {
    if [ -n "$TMPDIR_SEKIO" ] && [ -d "$TMPDIR_SEKIO" ]; then
        rm -rf "$TMPDIR_SEKIO"
    fi
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
trap 'cleanup; exit 129' HUP

usage() {
    cat >&2 <<'EOF'
sekio installer

USAGE
    install.sh [OPTIONS]
    curl -fsSL https://raw.githubusercontent.com/hairbui76/sekio/main/install.sh | sh
    curl -fsSL https://raw.githubusercontent.com/hairbui76/sekio/main/install.sh | sh -s -- --version v0.1.0

OPTIONS
    --version <tag>   Install this release instead of the latest (e.g. v0.1.0).
    --prefix <dir>    Install into <dir> instead of ~/.local/bin.
    --uninstall       Remove the sekio binaries from the prefix and exit.
    -h, --help        Show this help.

ENVIRONMENT
    SEKIO_VERSION     Same as --version.
    SEKIO_PREFIX      Same as --prefix.
    GITHUB_TOKEN      Used for the release-lookup API call, if set. Only needed
                      when the unauthenticated rate limit bites.
EOF
}

# ------------------------------------------------------------- utilities ----

have() { command -v "$1" >/dev/null 2>&1; }

# Emit the URL's body on stdout. curl first, wget as a fallback.
fetch_stdout() {
    url="$1"
    if have curl; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            curl -fsSL --proto '=https' --tlsv1.2 \
                -H "Authorization: Bearer ${GITHUB_TOKEN}" "$url"
        else
            curl -fsSL --proto '=https' --tlsv1.2 "$url"
        fi
    elif have wget; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            wget -qO- --header="Authorization: Bearer ${GITHUB_TOKEN}" "$url"
        else
            wget -qO- "$url"
        fi
    else
        err "need curl or wget to download; neither is on PATH"
    fi
}

# Save the URL to a file.
fetch_file() {
    url="$1"
    dest="$2"
    if have curl; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$dest" "$url"
    elif have wget; then
        wget -qO "$dest" "$url"
    else
        err "need curl or wget to download; neither is on PATH"
    fi
}

# Print the lowercase SHA-256 of a file, hash only.
sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif have openssl; then
        openssl dgst -sha256 "$1" | sed 's/.*= *//'
    else
        err "no SHA-256 tool found (need sha256sum, shasum, or openssl).
       Refusing to install an unverified download."
    fi
}

lower() { tr '[:upper:]' '[:lower:]'; }

# ------------------------------------------------------------ arguments -----

version="${SEKIO_VERSION:-}"
prefix="${SEKIO_PREFIX:-}"
action="install"

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || err "--version needs an argument"
            version="$2"
            shift 2
            ;;
        --version=*)
            version="${1#--version=}"
            shift
            ;;
        --prefix)
            [ $# -ge 2 ] || err "--prefix needs an argument"
            prefix="$2"
            shift 2
            ;;
        --prefix=*)
            prefix="${1#--prefix=}"
            shift
            ;;
        --uninstall)
            action="uninstall"
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            say "unknown option: $1"
            usage
            exit 1
            ;;
    esac
done

[ -n "$prefix" ] || prefix="${HOME:-/root}/.local/bin"

# ------------------------------------------------------------ uninstall -----

if [ "$action" = "uninstall" ]; then
    removed=0
    for bin in $BINS; do
        if [ -e "$prefix/$bin" ]; then
            rm -f "$prefix/$bin" || err "could not remove $prefix/$bin"
            info "removed $prefix/$bin"
            removed=$((removed + 1))
        fi
    done
    if [ "$removed" -eq 0 ]; then
        info "nothing to remove in $prefix"
    else
        ok "uninstalled sekio from $prefix"
        say ""
        say "If you started the GUI daemon, stop it too:"
        say "    pkill -f 'sekio-gui --daemon'"
        say "Config and cache, if any, are left alone:"
        say "    ${XDG_CONFIG_HOME:-\$HOME/.config}/sekio"
    fi
    exit 0
fi

# ------------------------------------------------------------- platform -----

os=$(uname -s | lower)
case "$os" in
    linux) ;;
    darwin)
        err "macOS is not a supported target (see ROADMAP.md).
       Build from source with: cargo install --git https://github.com/${REPO} sekio-cli"
        ;;
    mingw* | msys* | cygwin*)
        err "this script installs the Linux build.
       On Windows use the .msi from https://github.com/${REPO}/releases
       or: scoop install sekio"
        ;;
    *)
        err "unsupported OS: $(uname -s)"
        ;;
esac

machine=$(uname -m)
case "$machine" in
    x86_64 | amd64) arch="x86_64" ;;
    *)
        # Releases are x86_64-only. Say which architectures exist and how to
        # get one built, rather than 404ing on an asset that was never
        # published.
        err "unsupported architecture: $machine
       sekio publishes x86_64 Linux builds only. Build from source instead:
       cargo install --git https://github.com/${REPO} sekio-cli
       cargo install --git https://github.com/${REPO} sekio-tui
       cargo install --git https://github.com/${REPO} sekio-gui"
        ;;
esac

target="${arch}-unknown-linux-gnu"
asset="sekio-${target}.tar.gz"
expected_bins="sekio sekio-tui sekio-gui"

# -------------------------------------------------------------- version -----

if [ -z "$version" ]; then
    info "resolving latest release..."
    version=$(fetch_stdout "$API_URL" |
        sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
        head -n 1) || true
    [ -n "$version" ] || err "could not resolve the latest release tag from
       $API_URL
       Pass one explicitly: --version v0.1.0 (or set SEKIO_VERSION)."
fi

# Accept "0.1.0" as well as "v0.1.0".
case "$version" in
    v*) ;;
    *) version="v${version}" ;;
esac

info "installing sekio ${version} (${target}) into ${prefix}"

# ------------------------------------------------------------- download -----

TMPDIR_SEKIO=$(mktemp -d 2>/dev/null || mktemp -d -t sekio) ||
    err "could not create a temporary directory"

archive="${TMPDIR_SEKIO}/${asset}"
sumfile="${TMPDIR_SEKIO}/${asset}.sha256"
url="${BASE_URL}/${version}/${asset}"

info "downloading ${url}"
fetch_file "$url" "$archive" || err "download failed: $url
       Does the release ${version} exist, and does it have ${asset}?"
[ -s "$archive" ] || err "downloaded archive is empty: $url"

info "downloading ${url}.sha256"
fetch_file "${url}.sha256" "$sumfile" || err "could not download the checksum file:
       ${url}.sha256
       Refusing to install an unverified download."

# --------------------------------------------------------------- verify -----
# This is the part that makes a curl-pipe-sh installer safe to run. A published
# checksum only helps if it is actually checked, before anything is unpacked.

# The release publishes a bare hash; accept "<hash>  <name>" form too.
expected=$(tr -d '\r' <"$sumfile" | head -n 1 | tr -s ' \t' ' ' | cut -d' ' -f1 | lower)
actual=$(sha256_of "$archive" | lower)

case "$expected" in
    [0-9a-f][0-9a-f]*) ;;
    *) err "checksum file did not contain a SHA-256: ${url}.sha256" ;;
esac
[ "${#expected}" -eq 64 ] || err "checksum file did not contain a 64-hex-digit SHA-256"

if [ "$expected" != "$actual" ]; then
    err "CHECKSUM MISMATCH — refusing to install.

       file:     ${asset}
       expected: ${expected}
       actual:   ${actual}

       The download does not match the checksum published with the release.
       This means a corrupted transfer or a tampered artifact. Nothing was
       extracted or installed. Report this at
       https://github.com/${REPO}/issues if it reproduces."
fi
info "checksum ok (${actual})"

# -------------------------------------------------------------- extract -----

extract="${TMPDIR_SEKIO}/extract"
mkdir -p "$extract"
tar -xzf "$archive" -C "$extract" || err "could not extract $asset"

mkdir -p "$prefix" || err "could not create $prefix"
[ -w "$prefix" ] || err "$prefix is not writable.
       Pick another prefix: --prefix \"\$HOME/.local/bin\""

installed=""
missing=""
for bin in $expected_bins; do
    src="${extract}/${bin}"
    if [ ! -f "$src" ]; then
        missing="${missing} ${bin}"
        continue
    fi
    # Install to a temp name and rename, so a running sekio isn't clobbered
    # mid-write (rename is atomic; overwriting a busy binary is not).
    tmp="${prefix}/.${bin}.new.$$"
    cp "$src" "$tmp" || err "could not write to $prefix"
    chmod 0755 "$tmp"
    mv -f "$tmp" "${prefix}/${bin}" || err "could not install ${prefix}/${bin}"
    installed="${installed} ${bin}"
done

[ -n "$installed" ] || err "the archive contained none of: $expected_bins"
if [ -n "$missing" ]; then
    warn "these binaries were missing from ${asset}:${missing}"
fi

ok "installed${installed} to ${prefix}"

# ----------------------------------------------------------------- notes -----


# --------------------------------------------------------------- $PATH ------

case ":${PATH:-}:" in
    *":${prefix}:"*) on_path=1 ;;
    *) on_path=0 ;;
esac

if [ "$on_path" -eq 0 ]; then
    say ""
    warn "${prefix} is not on your \$PATH."
    # SC2088: the tilde is deliberately literal here — these strings are shown
    # to the user as the file to edit, never opened by this script.
    # shellcheck disable=SC2088
    case "${SHELL:-}" in
        */fish) rc="~/.config/fish/config.fish" line="fish_add_path ${prefix}" ;;
        */zsh) rc="~/.zshrc" line="export PATH=\"${prefix}:\$PATH\"" ;;
        */bash) rc="~/.bashrc" line="export PATH=\"${prefix}:\$PATH\"" ;;
        *) rc="your shell's rc file" line="export PATH=\"${prefix}:\$PATH\"" ;;
    esac
    say "  Add it by appending this to ${rc}:"
    say ""
    say "      ${line}"
    say ""
    say "  then restart your shell (or run the line now for this session)."
else
    say ""
    say "Try it:"
    say "      sekio --help"
    case "$installed" in
        *sekio-gui*)
            say "      sekio-gui --daemon &     # keep a warm instance for instant popups"
            ;;
    esac
fi

say ""
say "Uninstall with:"
say "      curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | sh -s -- --uninstall"
