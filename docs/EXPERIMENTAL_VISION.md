# Experimental WDA text recognition

Apply `patches/wda-vision.patch` **after** the bundled turbo patch on Appium WebDriverAgent 16.14.0. It adds two aliases, `POST /wda/fastOCR` and `POST /wda/fastRead`, both with and without a session. The Python reader still uses its existing OCR path.

Send `{}` or `{"mode":"fast"}` for Vision's fast recognizer with language correction disabled. Use `{"mode":"accurate"}` for the accurate recognizer with language correction enabled. Other modes and keys return a WDA invalid argument error. Both routes capture the full main screen once through XCTest's public `XCUIScreen.screenshot.PNGRepresentation` and run one public `VNRecognizeTextRequest` locally. The screenshot stays in the runner. The caller receives recognized text, which may be sensitive.

The WDA response wraps `{"mode":"fast","lines":[]}` in its normal `value` field. A successful blank screen returns an empty array. Each observation contributes its best candidate with `text`, Vision `confidence`, and `x`, `y`, `width`, `height`. The bounding box uses normalized fractions of the oriented screenshot from its **top left**. Multiply horizontal values by the displayed screenshot width and vertical values by its height to convert to the corresponding pixels or points. Lines follow Vision's observation order. Capture and recognition failures return WDA errors.

## Build without signing or running

On a Mac with Xcode selected, run the following in Bash from this TurboTap checkout. Set `WDA_REPO` to a local WDA Git clone containing the pinned commit. This uses a new scratch checkout and DerivedData directory. It also produces the cumulative patch required by the deploy script.

```bash
set -euo pipefail
turbo=$PWD
pin=d17782422d55ff1e5e0ceb74eb1fd509cc0c35b6
scratch=$(mktemp -d "${TMPDIR:-/var/tmp}/turbotap-vision.XXXXXX")
git clone --quiet --no-hardlinks --no-checkout "${WDA_REPO:?local WDA clone required}" "$scratch/source"
git -C "$scratch/source" checkout --quiet --detach "$pin"
git -C "$scratch/source" apply "$turbo/patches/wda-turbo.patch"
git -C "$scratch/source" apply "$turbo/patches/wda-vision.patch"
git -C "$scratch/source" diff --binary HEAD > "$scratch/wda-with-vision.patch"
(
  cd "$scratch/source"
  xcodebuild build-for-testing -project WebDriverAgent.xcodeproj \
    -scheme WebDriverAgentRunner -destination 'generic/platform=iOS' \
    -derivedDataPath "$scratch/DerivedData" CODE_SIGNING_ALLOWED=NO
)
```

## Later deployment and rollback

These commands are for a separately authorized device deployment. Use the configured TurboTap control host and a single deployment at a time. Copy the generated cumulative patch to that host and set `COMBINED_PATCH` to its absolute path. In a TurboTap checkout run

```bash
WDA_PATCH="${COMBINED_PATCH:?set absolute cumulative patch path}" \
  WDA_SRC=/var/tmp/WebDriverAgent-turbotap-vision \
  WDA_DD=/var/tmp/wda-dd-turbotap-vision \
  bash scripts/deploy_wda_mac.sh
```

The script signs and replaces the running WDA. Keep the previous deployment configuration and its exact patch before proceeding. For rollback to this checkout's bundled turbo build, run the following with the original device and signing configuration. Use fresh paths for the rollback source and build products so a previously applied overlay cannot survive the rebuild.

```bash
rollback=turbo-rollback-$(date +%s)
WDA_PATCH="$PWD/patches/wda-turbo.patch" \
  WDA_SRC="/var/tmp/WebDriverAgent-$rollback" WDA_DD="/var/tmp/wda-dd-$rollback" \
  bash scripts/deploy_wda_mac.sh
```

If the previous runner used another revision or patch, use that saved revision and patch instead. At a future authorized runtime check, verify the original runner is healthy and the experimental POST route again returns unknown command. This experiment measures neither recognition accuracy nor end-to-end latency.
