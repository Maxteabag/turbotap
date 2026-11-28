#!/usr/bin/env bash
# Run by hand from a maintainer checkout. Attach binaries to an existing release.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
repo=Maxteabag/turbotap
tag=${1:-}
if [[ $# -ne 1 || ! $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    printf 'Usage: %s vMAJOR.MINOR.PATCH\n' "$0" >&2
    exit 2
fi
release_commit=$(gh api "repos/$repo/commits/$tag" --jq .sha)
local_commit=$(git rev-parse --verify "refs/tags/$tag^{commit}")
if [[ $release_commit != "$local_commit" ]]; then
    printf 'Local %s does not match the published tag.\n' "$tag" >&2
    exit 1
fi
version=$(git show "$release_commit:Cargo.toml" | sed -n 's/^version = "\([^"]*\)"$/\1/p')
[[ $tag == "v$version" ]] || { printf 'Tag does not match Cargo.toml.\n' >&2; exit 1; }
scratch=$(mktemp -d "${TMPDIR:-/tmp}/turbotap-release.XXXXXXXX")
trap 'rm -rf "$scratch"' EXIT
assets=$(gh release view "$tag" --repo "$repo" --json assets --jq '.assets[] | [.name,.digest] | @tsv')

asset_digest() {
    awk -F '\t' -v name="$1" '$1 == name { print $2 }' <<< "$assets"
}
verify_digest() {
    local name=$1 expected=$2 actual
    actual=$(sha256sum < "$scratch/$name")
    [[ $expected == "sha256:${actual%% *}" ]] || {
        printf 'Published digest does not match %s.\n' "$name" >&2
        exit 1
    }
}

linux=turbotap-linux-x86_64
macos=turbotap-macos-arm64
pending=()
for asset in "$linux" "$macos"; do
    if awk -F '\t' -v name="$asset" '$1 == name { found = 1 } END { exit !found }' <<< "$assets"; then
        gh release download "$tag" --repo "$repo" --pattern "$asset" --dir "$scratch"
        verify_digest "$asset" "$(asset_digest "$asset")"
    else
        pending+=("$scratch/$asset")
        if [[ $asset == "$linux" ]]; then
            mkdir "$scratch/source"
            git archive "$release_commit" | tar -xf - -C "$scratch/source"
            buildbox-cargo --fetch release/turbotap --out "$scratch" "$scratch/source" build --locked --release
            mv "$scratch/turbotap" "$scratch/$linux"
            [[ $("$scratch/$linux" --version) == "turbotap $version" ]]
        else
            scripts/build_release_macos.sh "$scratch" "$release_commit" "$version"
        fi
    fi
done
file "$scratch/$linux" | grep -q 'ELF 64-bit.*x86-64'
file "$scratch/$macos" | grep -q 'Mach-O 64-bit executable arm64'
(cd "$scratch" && sha256sum "$linux" "$macos" > SHA256SUMS && sha256sum -c SHA256SUMS)

# Preserve existing binaries. Publish the checksum last, after both assets exist.
if (( ${#pending[@]} )); then
    gh release upload "$tag" "${pending[@]}" --repo "$repo"
fi
assets=$(gh release view "$tag" --repo "$repo" --json assets --jq '.assets[] | [.name,.digest] | @tsv')
verify_digest "$linux" "$(asset_digest "$linux")"
verify_digest "$macos" "$(asset_digest "$macos")"
manifest_digest=$(asset_digest SHA256SUMS)
local_digest=$(sha256sum < "$scratch/SHA256SUMS")
if [[ $manifest_digest != "sha256:${local_digest%% *}" ]]; then
    gh release upload "$tag" "$scratch/SHA256SUMS" --clobber --repo "$repo"
fi
assets=$(gh release view "$tag" --repo "$repo" --json assets --jq '.assets[] | [.name,.digest] | @tsv')
verify_digest SHA256SUMS "$(asset_digest SHA256SUMS)"
gh release view "$tag" --repo "$repo" --json url --jq .url
