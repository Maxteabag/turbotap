# Experimental WDA text recognition

Apply `patches/wda-vision.patch` **after** the bundled turbo patch on Appium WebDriverAgent 16.14.0. It adds `POST /wda/fastOCR` for OCR alone and `POST /wda/fastRead` for OCR plus fastUI in one request. Both support session and sessionless URLs. The Python reader still uses its existing OCR path.

Send `{}` or `{"mode":"fast"}` for Vision's fast recognizer with language correction disabled. Use `{"mode":"accurate"}` for the accurate recognizer with language correction enabled. Both routes capture the full main screen once with XCTest and run one public `VNRecognizeTextRequest` locally. The screenshot stays in the runner. The caller receives recognized text, which may be sensitive.

Choose the capture path in the POST body. Compare these three requests on the same screen with the same recognition mode.

```json
{"mode":"accurate","capture":"png"}
```

```json
{"mode":"accurate","capture":"cgimage"}
```

```json
{"mode":"accurate","capture":"cgimage","scale":0.75}
```

The default `png` uses `XCUIScreenshot.PNGRepresentation` and Vision's data initializer. The opt-in `cgimage` reads `XCUIScreenshot.image.CGImage` and passes pixels to Vision. XCTest controls how it produces the underlying screenshot. The runner redraws only for orientation correction or a change in pixel dimensions. The optional linear dimension scale is greater than zero and at most one, and defaults to one. PNG uses full resolution. Top-left normalized OCR boxes use the same coordinate system at every scale.

`value.image` reports the selected capture path, scale, oriented `sourcePixels` and `inputPixels` ([width, height]), and explicit `pngBytes` (zero for CGImage). `timing.startUptimeMs` is a device monotonic timestamp. Within `captureMs`, `screenshotMs` measures the XCTest screenshot call and `representationMs` measures fetching the image or PNG. After AX collection, `inputPrepMs` measures input setup including any redraw, PNG size inspection, and Vision handler construction. Vision may defer decoding until recognition. For latency comparisons use `totalMs` alongside the individual stages and digit hit counts.

Both responses include `value.mode`, `value.lines`, and `value.timing`. A successful blank screen returns an empty array. Each observation contributes its best candidate with `text`, Vision `confidence`, and `x`, `y`, `width`, `height`. The bounding box uses normalized fractions of the oriented screenshot from its **top left**. Multiply horizontal values by the displayed screenshot width and vertical values by its height to convert to the corresponding pixels or points. Lines follow Vision's observation order. Capture and recognition failures return WDA errors.

On a fused read, the runner captures the screenshot, collects the same AX payload as `GET /wda/fastUI`, then recognizes the captured image. The object at `value.fastUI` includes `ui`, `screen`, app metadata and keyboard state in the original fastUI schema and point coordinate system. The fused route accepts the existing fastUI query parameters. For example, `POST /wda/fastRead?text=1` with body `{"mode":"accurate"}` includes AX static text. A failed AX snapshot fails the fused request. Capture and AX collection are sequential, so an app transition can yield different screen states between them. An optional fastUI bundle wait occurs after screenshot capture.

`value.timing` reports monotonic elapsed milliseconds. `captureMs` includes screenshot capture and PNG creation, `axMs` includes the fastUI collection and optional wait (zero for fastOCR), and `recognitionMs` includes Vision setup, recognition and extraction of the line results. `totalMs` spans those operations up to result assembly. The caller measures HTTP round-trip time separately.

## Enable Vision from an installed Python package

Use the Linux control host where TurboTap is installed. The commands below
assume a configured device with a working TurboTap runner and its previous build still on the Mac.
Keep the previous device, signing, source and DerivedData settings for rollback. Run one deployment
at a time because the deploy script uses a shared patch staging path on the Mac.

From Bash, resolve the installed executable (including the symlink created by `uv tool install`).
Use its environment’s interpreter in isolated mode. Verify that the launcher belongs to that
interpreter’s scripts directory and that the installed distribution owns both patches.
Then combine those assets in a disposable checkout of the pinned upstream revision.

```bash
set -euo pipefail
launcher=$(realpath "$(type -P turbotap)")
IFS= read -r shebang < "$launcher"
tool_python=${shebang#\#!}
[[ "$tool_python" = /* && -x "$tool_python" ]] || {
  echo "Expected a Python console script with an absolute interpreter path: $launcher" >&2
  exit 1
}
assets=$("$tool_python" -I - "$launcher" <<'PYTHON'
from importlib.metadata import distribution
from pathlib import Path
import sys
import sysconfig
from turbotap.cli import asset_dir

launcher = Path(sys.argv[1])
assert launcher.samefile(Path(sysconfig.get_path("scripts")) / "turbotap")
dist = distribution("turbotap")
owned = {dist.locate_file(f).resolve() for f in dist.files or ()}
patches = Path(asset_dir("patches")).resolve()
for name in ("wda-turbo.patch", "wda-vision.patch"):
    patch = patches / name
    assert patch in owned and patch.is_file(), f"missing installed asset: {patch}"
print(patches)
PYTHON
)
"$launcher" vision-ocr --help | grep -F -- '--scale'
pin=d17782422d55ff1e5e0ceb74eb1fd509cc0c35b6
scratch=$(mktemp -d)
git clone -q --no-checkout https://github.com/appium/WebDriverAgent.git "$scratch/source"
git -C "$scratch/source" checkout -q --detach "$pin"
git -C "$scratch/source" apply --index "$assets/wda-turbo.patch"
git -C "$scratch/source" apply --index "$assets/wda-vision.patch"
git -C "$scratch/source" diff --cached --binary HEAD > "$scratch/vision.patch"
```

Build in separate Mac directories and start the Vision runner on the configured device. This
replaces the running WDA session. The environment overrides apply only to this deployment.

```bash
run=$(basename "$scratch")
WDA_REF="$pin" WDA_PATCH="$scratch/vision.patch" \
  WDA_SRC="/var/tmp/wda-vision-src-$run" WDA_DD="/var/tmp/wda-vision-dd-$run" \
  "$launcher" deploy
"$launcher" vision-ocr --mode accurate --scale 0.5
```

The last command verifies that the runner confirms direct CGImage input at half width and
height. An explicit scale of 1 uses full-size CGImage. Omit the flag to use full-size PNG,
including on older Vision overlays. On one multilingual device fixture, accurate recognition
at half size recovered 72 of 72 expected matches, compared with 66 at full size. Choose a
scale based on the text size and content of your screens.

To restore the previous TurboTap runner, use its original environment and the preserved build.
Require both a successful stock read with session and screen dimensions and the specific
unsupported-route error from the same device. Connection failures and recognition errors
fail this check.

```bash
"$launcher" deploy --no-build
"$launcher" preflight | "$tool_python" -I -c '
import json, sys
r = json.load(sys.stdin)
assert r["ok"] and r["session_id"] and len(r["screen_pt"]) == 2
assert all(n > 0 for n in r["screen_pt"])
'
if "$launcher" vision-ocr > /dev/null 2>"$scratch/rollback.err"; then
  echo "Rollback failed: Vision OCR is still available" >&2
  exit 1
fi
grep -F 'WDA lacks /wda/fastOCR.' "$scratch/rollback.err"
```

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
