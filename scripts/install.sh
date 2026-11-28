#!/bin/sh
# Install the published Turbotap binary for this machine.
set -eu

# Keep the entry point at the end so a truncated piped download cannot run it.
main() {
    case "$(uname -s):$(uname -m)" in
        Linux:x86_64) asset=turbotap-linux-x86_64 ;;
        Darwin:arm64) asset=turbotap-macos-arm64 ;;
        *) printf '%s\n' 'No prebuilt Turbotap for this platform. Install from source with Cargo.' >&2; exit 1 ;;
    esac

    dest=${TURBOTAP_INSTALL_DIR:-"$HOME/.local/bin"}
    mkdir -p "$dest"
    dest=$(cd "$dest" && pwd -P)
    scratch=$(mktemp -d "$dest/.turbotap-install.XXXXXXXX")
    trap 'rm -rf "$scratch"' EXIT
    trap 'exit 1' HUP INT TERM

    # Resolve latest once, since the stable download URL can change between requests.
    release_url=$(curl --proto '=https' --proto-redir '=https' -fsSL -o /dev/null \
        -w '%{url_effective}' https://github.com/Maxteabag/turbotap/releases/latest)
    case "$release_url" in
        https://github.com/Maxteabag/turbotap/releases/tag/v*) tag=${release_url##*/} ;;
        *) printf 'Unexpected release URL: %s\n' "$release_url" >&2; exit 1 ;;
    esac
    if ! printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
        printf 'Invalid release tag: %s\n' "$tag" >&2
        exit 1
    fi

    downloads="https://github.com/Maxteabag/turbotap/releases/download/$tag"
    curl --proto '=https' --proto-redir '=https' -fsSL "$downloads/SHA256SUMS" -o "$scratch/SHA256SUMS"
    curl --proto '=https' --proto-redir '=https' -fsSL "$downloads/$asset" -o "$scratch/turbotap"
    expected=$(awk -v asset="$asset" '
        $2 == asset && NF == 2 && length($1) == 64 && $1 !~ /[^0-9a-f]/ { hash = $1; count++ }
        END { if (count != 1) exit 1; print hash }
    ' "$scratch/SHA256SUMS") || { printf 'Missing or ambiguous checksum for %s\n' "$asset" >&2; exit 1; }
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum < "$scratch/turbotap")
    else
        actual=$(shasum -a 256 < "$scratch/turbotap")
    fi
    if [ "${actual%% *}" != "$expected" ]; then
        printf 'Checksum mismatch for %s\n' "$asset" >&2
        exit 1
    fi
    chmod 0755 "$scratch/turbotap"
    if [ -d "$dest/turbotap" ]; then
        printf 'Destination is a directory: %s/turbotap\n' "$dest" >&2
        exit 1
    fi
    mv -f "$scratch/turbotap" "$dest/turbotap"
    printf 'Installed Turbotap %s to %s/turbotap\n' "$tag" "$dest"
    case ":${PATH:-}:" in
        *":$dest:"*) ;;
        *) printf 'Add %s to PATH or invoke %s/turbotap directly.\n' "$dest" "$dest" ;;
    esac
}

main "$@"
