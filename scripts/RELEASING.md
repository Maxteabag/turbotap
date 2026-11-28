# Publish prebuilt Turbotap binaries

Run the release script by hand on Linux. It uses `buildbox-cargo` on ai-swarm for Linux x86-64 and SSH to an Apple Silicon Mac for the macOS arm64 build. Set `TURBOTAP_MAC_HOST` if your Mac SSH alias differs from `mac`. The Mac needs Xcode command-line tools. If Cargo is unavailable, the script installs a minimal Rust toolchain in its disposable build directory.

Use a reviewed commit and a verified release tag. For a new release, push the tag and create the GitHub release as a draft, then attach and verify the assets before publishing the draft.

```sh
git fetch origin --tags
gh release create vMAJOR.MINOR.PATCH --verify-tag --draft --notes-file release-notes.md
./scripts/release.sh vMAJOR.MINOR.PATCH
gh release edit vMAJOR.MINOR.PATCH --draft=false
```

For a backfill, run just the release script against the existing release. It verifies and preserves any binary already attached, builds missing binaries from the exact published tag, and publishes a separate `<binary>.SHA256SUMS` for each platform. Existing manifests, including the legacy `SHA256SUMS` on v1.6.7, stay available. The script prints the release URL after checking GitHub’s asset digests.

## Install the latest published release

```sh
curl -fsSL https://raw.githubusercontent.com/Maxteabag/turbotap/main/scripts/install.sh | sh
```

The installer uses the latest stable release tag for both the binary and its checksum. It installs to `~/.local/bin/turbotap` by default. Set `TURBOTAP_INSTALL_DIR` in the environment of `sh` to choose another directory. The Linux v1.6.7 binary requires x86-64 with glibc 2.39 or later. The installer runs the staged binary to verify it starts and reports the requested version before replacing any installation. Older glibc, musl and other unsupported platforms can use the Cargo source install.
