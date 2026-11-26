#!/usr/bin/env bash
# Build and (re)start the wda-turbo patched WebDriverAgent on a remote Mac host.
#   turbotap deploy             # clone/update, patch, build, restart runner, wait for WDA
#   turbotap deploy --no-build  # only restart the runner from the existing build
# (turbotap runs this script; it also runs it directly: bash deploy_wda_mac.sh [--no-build])
#
# Configuration via env vars or ~/.config/turbotap/config.json:
#   WDA_URL       e.g. http://192.168.1.50:8100
#   WDA_UDID      target iOS device UDID (or IPHONE_UDID)
#   WDA_MAC_HOST  SSH alias of the Mac running xcodebuild (default: mac)
#   WDA_TEAM      Apple Developer Team ID
#   WDA_BUNDLE    WebDriverAgentRunner bundle identifier
#   WDA_DEVICE    optional device key in ~/.config/turbotap/config.json
#   WDA_KEYCHAIN_DIR  dir on the Mac with wda.keychain-db + kcpass, a dedicated signing
#                     keychain unlocked over SSH (default ~/.local/share/iphone-use/signing)
set -euo pipefail
HERE="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
PATCH="${WDA_PATCH:-$HERE/../patches/wda-turbo.patch}"
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
    echo "Missing $req. Set it in the environment or configure ~/.config/turbotap/config.json" >&2
    exit 2
  fi
done

# The runner listens on the configured URL's port (not a fixed 8100).
PORT="${WDA_URL##*:}"; PORT="${PORT%%/*}"; [[ "$PORT" =~ ^[0-9]+$ ]] || PORT=8100
# What answers before the restart, so the receipt can tell a new runner from the old one.
wda_json() { { curl -fsS --max-time 3 "$WDA_URL$1" 2>/dev/null || true; } | python3 -c 'import json,sys
try: d = json.load(sys.stdin)
except Exception: sys.exit(0)
v = d.get("value") if isinstance(d.get("value"), dict) else {}
print(d.get("sessionId") or v.get("sessionId") or "", v.get("bundleId") or "", v.get("pid", ""), "answered", sep="|")' || true; }
OLD_SID="$(wda_json /status | cut -d"|" -f1)"
RUN_OUT="$(mktemp /var/tmp/wda-deploy.XXXXXX)"; trap 'rm -f "$RUN_OUT"' EXIT
scp -q "$PATCH" "$MAC:/var/tmp/wda-turbo.patch"
ssh "$MAC" BUILD="$BUILD" UDID="$UDID" PORT="$PORT" TEAM="$TEAM" BUNDLE="$BUNDLE" REF="$UPSTREAM_REF" SRC="$SRC" DD="$DD" \
  KCDIR="${WDA_KEYCHAIN_DIR:-}" XB="${WDA_XCODEBUILD:-}" bash -s <<'REMOTE' | tee "$RUN_OUT"
set -euo pipefail
[[ -n "$XB" && -x "$XB" ]] || XB=/Applications/Xcode-26.6.app/Contents/Developer/usr/bin/xcodebuild
[[ -x "$XB" ]] || XB=xcodebuild
KCDIR="${KCDIR:-$HOME/.local/share/iphone-use/signing}"
KC="$KCDIR/wda.keychain-db"
PROV="$KCDIR/wda.mobileprovision"
SIGN=(DEVELOPMENT_TEAM="$TEAM" CODE_SIGN_IDENTITY="Apple Development" PRODUCT_BUNDLE_IDENTIFIER="$BUNDLE")
if [[ -f "$KC" && -f "$KCDIR/kcpass" ]]; then
  # Over ssh the keychain is locked and codesign fails with errSecInternalComponent.
  security unlock-keychain -p "$(cat "$KCDIR/kcpass")" "$KC"
  security set-keychain-settings -t 3600 "$KC"
  # Ensure Xcode's provisioning matcher can see the signing identity in $KC.
  if ! security list-keychains -d user | grep -Fq "$KC"; then
    EXISTING_KCS=()
    while IFS= read -r line; do
      line="${line#*\"}"; line="${line%\"*}"
      [[ -n "$line" ]] && EXISTING_KCS+=("$line")
    done < <(security list-keychains -d user)
    security list-keychains -d user -s "$KC" "${EXISTING_KCS[@]}"
  fi
  SIGN+=(OTHER_CODE_SIGN_FLAGS="--keychain $KC")
fi
if [[ -f "$PROV" ]]; then
  UUID="$(security cms -D -i "$PROV" 2>/dev/null | plutil -extract UUID raw -o - - 2>/dev/null || true)"
  if [[ -n "$UUID" ]]; then
    for pdir in "$HOME/Library/MobileDevice/Provisioning Profiles" "$HOME/Library/Developer/Xcode/UserData/Provisioning Profiles"; do
      mkdir -p "$pdir"
      cp -f "$PROV" "$pdir/$UUID.mobileprovision"
    done
  fi
else
  SIGN+=(-allowProvisioningUpdates CODE_SIGN_STYLE=Automatic)
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
  git checkout -q -- WebDriverAgent.xcodeproj/project.pbxproj
  if [[ -n "${UUID:-}" ]]; then
    # Scope manual provisioning profile strictly to WebDriverAgentRunner (never WebDriverAgentLib framework).
    python3 - "$UUID" <<'PYEOF'
import re, sys
uuid = sys.argv[1]
p = "WebDriverAgent.xcodeproj/project.pbxproj"
txt = open(p).read()
def patch_runner(m):
    blk = m.group(0)
    blk = re.sub(r"CODE_SIGN_STYLE\s*=\s*[^;]+;", "CODE_SIGN_STYLE = Manual;", blk)
    if "CODE_SIGN_STYLE" not in blk:
        blk = blk.replace("INFOPLIST_FILE = WebDriverAgentRunner/Info.plist;",
                          f'CODE_SIGN_STYLE = Manual;\n\t\t\t\tPROVISIONING_PROFILE_SPECIFIER = "{uuid}";\n\t\t\t\tINFOPLIST_FILE = WebDriverAgentRunner/Info.plist;')
    elif "PROVISIONING_PROFILE_SPECIFIER" not in blk:
        blk = blk.replace("CODE_SIGN_STYLE = Manual;",
                          f'CODE_SIGN_STYLE = Manual;\n\t\t\t\tPROVISIONING_PROFILE_SPECIFIER = "{uuid}";')
    return blk
txt2 = re.sub(r"buildSettings = \{[^{}]*INFOPLIST_FILE = WebDriverAgentRunner/Info\.plist;[^{}]*\};", patch_runner, txt)
open(p, "w").write(txt2)
PYEOF
  fi
  if ! "$XB" build-for-testing -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner \
      -destination generic/platform=iOS -derivedDataPath "$DD" "${SIGN[@]}" >/var/tmp/wda-turbo-build.log 2>&1; then
    if [[ -n "${UUID:-}" ]] && grep -q "ProcessProductPackaging" /var/tmp/wda-turbo-build.log; then
      for pdir in "$HOME/Library/MobileDevice/Provisioning Profiles" "$HOME/Library/Developer/Xcode/UserData/Provisioning Profiles"; do
        mkdir -p "$pdir"
        cp -f "$PROV" "$pdir/$UUID.mobileprovision"
      done
      "$XB" build-for-testing -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner \
        -destination generic/platform=iOS -derivedDataPath "$DD" "${SIGN[@]}" >/var/tmp/wda-turbo-build.log 2>&1 \
        || { tail -25 /var/tmp/wda-turbo-build.log; exit 1; }
    else
      tail -25 /var/tmp/wda-turbo-build.log; exit 1
    fi
  fi
  echo "build ok"
fi
# Stop any WebDriverAgentRunner test session for this device (no pkill -f).
for pid in $(ps -axo pid=,command= | awk -v u="$UDID" '/xcodebuild/ && /WebDriverAgentRunner/ && /test-without-building/ && index($0,u) {print $1}'); do
  kill "$pid" 2>/dev/null || true
done
sleep 2
cd "$SRC"
# One log per device, kept across restarts (the previous run moves to .1), so a
# restart never wipes the evidence of why the last runner stopped working.
LOG="/var/tmp/wda-runner-$UDID.log"
[ -f "$LOG" ] && mv -f "$LOG" "$LOG.1"
nohup "$XB" test-without-building -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner \
  -destination "id=$UDID" -derivedDataPath "$DD" "${SIGN[@]}" USE_PORT="$PORT" \
  >"$LOG" 2>&1 </dev/null &
echo "runner started pid $! (log $LOG)"
echo "RUNNER_PID=$!"
REMOTE
RPID="$(sed -n 's/^RUNNER_PID=//p' "$RUN_OUT" | tail -1)"
LOG="/var/tmp/wda-runner-$UDID.log"
[[ -n "$RPID" ]] || { echo "the Mac did not report a runner pid; see $MAC:$LOG" >&2; exit 1; }
# Receipt: 'up' only when the runner we just started is alive and bound to this UDID, the
# configured URL answers with a session other than the one before, and WDA can read the
# foreground app (a runner without UI-testing access answers screenshots but reports
# local.pid.0 / pid 0 there).
runner_ok() {
  local cmd; cmd="$(ssh "$MAC" "ps -o command= -p $RPID" 2>/dev/null)" || return 1
  [[ "$cmd" == *xcodebuild*test-without-building*"id=$UDID"* ]]
}
why="no answer on $WDA_URL"
for i in $(seq 1 60); do
  if (( i % 10 == 0 )) && ! runner_ok; then
    echo "WDA did not come up: runner pid $RPID for $UDID has exited; see $MAC:$LOG" >&2
    exit 1
  fi
  IFS="|" read -r sid _ _ answered <<<"$(wda_json /status)"
  if [[ "$answered" != answered ]]; then why="no answer on $WDA_URL"
  elif [[ -n "$sid" && "$sid" == "$OLD_SID" ]]; then why="the old runner still answers (session $sid)"
  elif [[ -n "$sid" || -z "$OLD_SID" ]]; then
    IFS="|" read -r _ bundle apid _ <<<"$(wda_json /wda/activeAppInfo)"
    if [[ -z "$bundle" || "$bundle" == local.pid.* || "$apid" == 0 ]]; then
      why="WDA answers but cannot read the foreground app (${bundle:-none}, pid ${apid:-?}): no UI-testing access"
    elif curl -fsS --max-time 2 -X POST -H 'Content-Type: application/json' -d '{"x0":0,"x1":320,"y0":0,"y1":40}' \
         "$WDA_URL/wda/fastScreenshot" -o /dev/null 2>/dev/null; then
      if runner_ok; then
        echo "wda-turbo up at $WDA_URL: runner pid $RPID for $UDID on $MAC, session ${sid:-new}, front app $bundle, log $MAC:$LOG"
        exit 0
      fi
      why="runner pid $RPID is not the one answering"
    else why="screenshots fail"
    fi
  fi
  sleep 1
done
# Runner alive but nothing answers: a locked phone keeps Xcode at "Testing started" until it
# is unlocked by hand (it then starts by itself). Ask the device (read-only) and the log.
if [[ "$why" == "no answer on $WDA_URL" ]] && runner_ok; then
  lock="$(ssh "$MAC" "xcrun devicectl device info lockState --device '$UDID' 2>/dev/null | grep -i passcodeRequired; grep -c 'to Continue' '$LOG' 2>/dev/null" 2>/dev/null || true)"
  if grep -qi 'passcodeRequired.*true' <<<"$lock" || grep -qx '[1-9][0-9]*' <<<"$(tail -1 <<<"$lock")"; then
    echo "WDA did not come up: the phone $UDID is locked. Xcode waits for an unlock (runner pid $RPID stays alive) and the runner starts by itself afterwards; see $MAC:$LOG" >&2
    exit 3
  fi
fi
echo "WDA did not come up after 60 s: $why; see $MAC:$LOG" >&2
exit 1
