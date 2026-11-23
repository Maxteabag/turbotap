#!/usr/bin/env bash
# Build and (re)start the wda-turbo patched WebDriverAgent on a remote Mac host.
#   deploy_wda_mac.sh            # clone/update, patch, build, restart runner, wait for /status
#   deploy_wda_mac.sh --no-build # only restart the runner from the existing build
#
# Configuration via env vars or ~/.config/iphone-wda/config.json:
#   WDA_URL       e.g. http://192.168.1.50:8100
#   WDA_UDID      target iOS device UDID (or IPHONE_UDID)
#   WDA_MAC_HOST  SSH alias of the Mac running xcodebuild (default: mac)
#   WDA_TEAM      Apple Developer Team ID
#   WDA_BUNDLE    WebDriverAgentRunner bundle identifier
#   WDA_DEVICE    optional device key in ~/.config/iphone-wda/config.json
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PATCH="$HERE/../patches/wda-turbo.patch"
CFG="${WDA_CONFIG:-}"
if [[ -z "$CFG" ]]; then
  if [[ -f "$HOME/.config/turbotap/config.json" ]]; then
    CFG="$HOME/.config/turbotap/config.json"
  else
    CFG="$HOME/.config/iphone-wda/config.json"
  fi
fi

if [[ -f "$CFG" ]]; then
  eval "$(python3 - "$CFG" "${WDA_DEVICE:-}" <<'PY'
import json, shlex, sys
cfg = json.load(open(sys.argv[1]))
dev_key = sys.argv[2] or cfg.get("default_device") or ""
dev = (cfg.get("devices") or {}).get(dev_key) or {}
pairs = {
    "CFG_MAC": cfg.get("mac_host", ""),
    "CFG_TEAM": cfg.get("team_id", ""),
    "CFG_BUNDLE": cfg.get("bundle_id", ""),
    "CFG_URL": dev.get("url", ""),
    "CFG_UDID": dev.get("udid", ""),
}
for k, v in pairs.items():
    if v:
        print(f"{k}={shlex.quote(str(v))}")
PY
)"
fi

MAC="${WDA_MAC_HOST:-${CFG_MAC:-mac}}"
UDID="${WDA_UDID:-${IPHONE_UDID:-${CFG_UDID:-}}}"
WDA_URL="${WDA_URL:-${CFG_URL:-}}"
TEAM="${WDA_TEAM:-${CFG_TEAM:-}}"
BUNDLE="${WDA_BUNDLE:-${CFG_BUNDLE:-}}"
SRC="${WDA_SRC:-/var/tmp/WebDriverAgent}"
DD="${WDA_DD:-/var/tmp/wda-dd}"
UPSTREAM_REF="${WDA_REF:-d17782422d55ff1e5e0ceb74eb1fd509cc0c35b6}"  # appium/WebDriverAgent 16.14.0
BUILD=1
[[ "${1:-}" == "--no-build" ]] && BUILD=0

for req in UDID WDA_URL TEAM BUNDLE; do
  if [[ -z "${!req}" ]]; then
    echo "Missing $req. Set WDA_$req or configure ~/.config/iphone-wda/config.json" >&2
    exit 2
  fi
done

scp -q "$PATCH" "$MAC:/var/tmp/wda-turbo.patch"
ssh "$MAC" BUILD="$BUILD" UDID="$UDID" TEAM="$TEAM" BUNDLE="$BUNDLE" REF="$UPSTREAM_REF" SRC="$SRC" DD="$DD" bash -s <<'REMOTE'
set -euo pipefail
XB=/Applications/Xcode-26.6.app/Contents/Developer/usr/bin/xcodebuild
[[ -x "$XB" ]] || XB=xcodebuild
KC="$HOME/.local/share/iphone-use/signing/wda.keychain-db"
SIGN=(-allowProvisioningUpdates DEVELOPMENT_TEAM="$TEAM" CODE_SIGN_STYLE=Automatic
      CODE_SIGN_IDENTITY="Apple Development" PRODUCT_BUNDLE_IDENTIFIER="$BUNDLE")
if [[ -f "$KC" && -f "$HOME/.local/share/iphone-use/signing/kcpass" ]]; then
  # Over ssh the keychain is locked and codesign fails with errSecInternalComponent.
  security unlock-keychain -p "$(cat "$HOME/.local/share/iphone-use/signing/kcpass")" "$KC"
  security set-keychain-settings -t 3600 "$KC"
  SIGN+=(OTHER_CODE_SIGN_FLAGS="--keychain $KC")
fi
if [[ "$BUILD" == 1 ]]; then
  if [[ ! -d "$SRC/.git" ]]; then
    git clone -q https://github.com/appium/WebDriverAgent.git "$SRC"
  fi
  cd "$SRC"
  if git apply --reverse --check /var/tmp/wda-turbo.patch 2>/dev/null; then
    echo "wda-turbo patch already applied"
  else
    git fetch -q origin 2>/dev/null || true
    git checkout -q --force "$REF"
    git apply /var/tmp/wda-turbo.patch
    echo "wda-turbo patch applied on $REF"
  fi
  "$XB" build-for-testing -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner \
    -destination generic/platform=iOS -derivedDataPath "$DD" "${SIGN[@]}" >/var/tmp/wda-turbo-build.log 2>&1 \
    || { tail -20 /var/tmp/wda-turbo-build.log; exit 1; }
  echo "build ok"
fi
# Stop any WebDriverAgentRunner test session for this device (no pkill -f).
for pid in $(ps -axo pid=,command= | awk -v u="$UDID" '/xcodebuild/ && /WebDriverAgentRunner/ && /test-without-building/ && index($0,u) {print $1}'); do
  kill "$pid" 2>/dev/null || true
done
sleep 2
cd "$SRC"
nohup "$XB" test-without-building -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner \
  -destination "id=$UDID" -derivedDataPath "$DD" "${SIGN[@]}" USE_PORT=8100 \
  >/var/tmp/wda-direct.log 2>&1 </dev/null &
echo "runner started pid $!"
REMOTE
for _ in $(seq 1 60); do
  if curl -fsS --max-time 2 -X POST -H 'Content-Type: application/json' -d '{"x0":0,"x1":320,"y0":0,"y1":40}' \
       "$WDA_URL/wda/fastScreenshot" -o /dev/null 2>/dev/null; then
    echo "wda-turbo up at $WDA_URL"; exit 0
  fi
  sleep 1
done
echo "WDA did not come up; see $MAC:/var/tmp/wda-direct.log" >&2
exit 1
