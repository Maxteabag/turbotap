#!/usr/bin/env bash
set -euo pipefail
out=$1
commit=$2
version=$3
mac_host=${TURBOTAP_MAC_HOST:-mac}
remote=$(ssh -o BatchMode=yes "$mac_host" 'umask 077; mktemp -d /tmp/turbotap-release.XXXXXXXX')
[[ $remote =~ ^/tmp/turbotap-release\.[a-zA-Z0-9]+$ ]] || exit 1
trap 'ssh -o BatchMode=yes "$mac_host" "rm -rf -- $remote" || true' EXIT
git archive "$commit" | ssh -o BatchMode=yes "$mac_host" "mkdir $remote/source && tar -xf - -C $remote/source"
ssh -o BatchMode=yes "$mac_host" "/bin/bash -s -- $remote $version" <<'MAC'
set -euo pipefail
test "$(uname -s)-$(uname -m)" = Darwin-arm64
cd "$1/source"
if [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi
if ! cargo -V >/dev/null 2>&1; then
    export CARGO_HOME="$1/cargo" RUSTUP_HOME="$1/rustup"
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$1/rustup-init.sh"
    sh "$1/rustup-init.sh" -y --profile minimal --no-modify-path >&2
    export PATH="$CARGO_HOME/bin:$PATH"
fi
cargo -V
rustc -vV
cargo build --locked --release --target aarch64-apple-darwin
bin=target/aarch64-apple-darwin/release/turbotap
test "$(lipo -archs "$bin")" = arm64
test "$("$bin" --version)" = "turbotap $2"
otool -L "$bin"
shasum -a 256 "$bin" > "$1/digest"
cat "$1/digest"
MAC
scp -q -o BatchMode=yes "$mac_host:$remote/source/target/aarch64-apple-darwin/release/turbotap" "$out/turbotap-macos-arm64"
digest=$(ssh -o BatchMode=yes "$mac_host" "cut -d' ' -f1 $remote/digest")
printf '%s  %s\n' "$digest" "$out/turbotap-macos-arm64" | sha256sum -c
