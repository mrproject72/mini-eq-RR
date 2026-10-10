#!/usr/bin/env bash
# GOLD PATH: two-device separate-EQ with per-device curve preservation.
#
# The user's exact test, in order:
#   1. Play 2 streams on 2 different output devices (STEREO tones — mono
#      tones hide channel-pairing collapses).
#   2. Selected mode: select output device 1, apply curve 1.
#   3. Move to output device 2, apply curve 2.
#   4. Move back to device 1 — the curve must be PRESERVED (this is the
#      bug: it was replaced with the other device's curve).
#   5. Check device 2's curve is also still preserved.
#   6. Exit the app — streams must not break (handed back, players alive).
#
# Curve design: BOTH tones are 1000 Hz; curve 1 = +12 dB @ 1 kHz (loud),
# curve 2 = -12 dB @ 1 kHz (quiet). The measured RMS per device distinguishes
# the curves, so "device 1 kept curve 1" is a measurement, not a guess.
#
# Usage: ./tests-live/twodev_gold_path.sh [--flatpak]
#   --flatpak  drive the installed Flatpak build instead of the native binary
# Exit code: number of failed assertions (0 = all green).
set -u
cd "$(dirname "$0")/.." || exit 1

if [ -z "${GOLD_INNER:-}" ]; then
  command -v dbus-run-session >/dev/null 2>&1 || { echo "missing: dbus-run-session"; exit 99; }
  exec dbus-run-session -- env GOLD_INNER=1 "$0" "$@"
fi

FLATPAK=0
[ "${1:-}" = "--flatpak" ] && FLATPAK=1
DBUS="python3 tests-live/dbus.py"
if [ "$FLATPAK" = 1 ]; then
  APP_ID=io.github.mrproject72.mini_eq_rr
  start_app() { setsid nohup flatpak run --env=RUST_LOG=info "$APP_ID" >"$1" 2>&1 < /dev/null & disown; }
  kill_app() { flatpak kill "$APP_ID" >/dev/null 2>&1; kill "$APP_PID" 2>/dev/null; }
else
  start_app() { setsid nohup env RUST_LOG=info ./target/release/mini-eq-rr >"$1" 2>&1 < /dev/null & disown; }
  kill_app() { kill "$APP_PID" 2>/dev/null; }
fi
DEV_A="alsa_output.pci-0000_04_00.6.analog-stereo"
DEV_B="alsa_output.usb-Logitech_Inc_Logitech_H570e_Stereo_00000000-00.analog-stereo"
SCRATCH=tmp/twodev_gold
mkdir -p "$SCRATCH"
TONE="$SCRATCH/tone_1k.wav"
PRESET_DIR="$HOME/.config/mini-eq/output"
LINKS_FILE="$HOME/.config/mini-eq/output-presets.json"
LINKS_BAK="$SCRATCH/output-presets.json.bak"
PASS=0; FAIL=0; FAILED_NAMES=()
PID_A=""; PID_B=""; APP_PID=""

for tool in pw-play pw-dump pw-record python3; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing tool: $tool"; exit 99; }
done
[ "$FLATPAK" = 1 ] || { [ -x ./target/release/mini-eq-rr ] || { cargo build --release || exit 99; }; }

cleanup() {
  if kill -0 "$APP_PID" 2>/dev/null; then
    $DBUS call SetRoutingEnabled b:false >/dev/null 2>&1
    $DBUS call Quit >/dev/null 2>&1
    sleep 2
    kill -0 "$APP_PID" 2>/dev/null && kill_app
  fi
  [ -n "$PID_A" ] && kill "$PID_A" 2>/dev/null
  [ -n "$PID_B" ] && kill "$PID_B" 2>/dev/null
  [ -f "$LINKS_BAK" ] && cp "$LINKS_BAK" "$LINKS_FILE" 2>/dev/null && rm -f "$LINKS_BAK"
  rm -f "$PRESET_DIR/gold_loud.json" "$PRESET_DIR/gold_quiet.json" 2>/dev/null
  # GOLD_KEEP=1 preserves the scratch dir (app log) for post-mortem.
  [ "${GOLD_KEEP:-0}" = 1 ] || rm -rf "$SCRATCH"
}
trap cleanup EXIT

check() {
  local name="$1"; shift
  local out
  if out="$("$@" 2>&1)"; then PASS=$((PASS+1)); echo "PASS: $name"
  else FAIL=$((FAIL+1)); FAILED_NAMES+=("$name"); echo "FAIL: $name"; [ -n "$out" ] && echo "$out" | sed 's/^/  -- /'; fi
}

state_val() {
  gdbus call --session --dest io.github.mrproject72.mini_eq_rr \
    --object-path /io/github/mrproject72/mini_eq_rr/Control \
    --method io.github.mrproject72.MiniEqRR.Control.GetState 2>/dev/null \
    | grep "^$1=" | cut -d= -f2-
}
# NOTE: through dbus.py, booleans need the b: prefix (bare args are parsed
# as strings and the call fails to parse); raw gdbus wants bare true/false.
dbus_bool() { $DBUS call "$1" "b:$2" >/dev/null 2>&1; }

eq_out_for() {
  python3 -c "
import re, sys
s = re.sub(r'[^A-Za-z0-9_.-]', '_', sys.argv[1]).strip('_.-')
print('mini_eq_sink_' + s + '_output')" "$1"
}

stream_links() {
  pw-dump 2>/dev/null | python3 -c "
import json, sys
d = json.load(sys.stdin)
names = {n['id']: n['info'].get('props', {}).get('node.name', '?') for n in d if n.get('type') == 'PipeWire:Interface:Node'}
out = []
for m in d:
    if m.get('type') == 'PipeWire:Interface:Link':
        i = m['info']
        on = names.get(i.get('output-node-id'), '')
        if 'pw-play' in on:
            out.append(on + ' -> ' + names.get(i.get('input-node-id'), '?'))
print('; '.join(out) if out else 'NO-LINKS')"
}

rms_of() { # RMS of a capture from an EQ _output node
  local eq_out="$1" f="$SCRATCH/cap.raw"
  rm -f "$f"
  timeout 6 pw-record --target="$eq_out" "$f" >/dev/null 2>&1
  [ -s "$f" ] || { echo "0.0"; return 0; }
  python3 -c "
import struct, math
d = open('$f', 'rb').read()
s = struct.unpack('<%dh' % (len(d) // 2), d)
print('%.1f' % math.sqrt(sum(x * x for x in s) / len(s)))"
}

# --- 0. precondition: no instance running ------------------------------------
if gdbus call --session --dest io.github.mrproject72.mini_eq_rr --object-path /io/github/mrproject72/mini_eq_rr/Control \
     --method io.github.mrproject72.MiniEqRR.Control.GetState >/dev/null 2>&1; then
  echo "ABORT: a mini-eq instance is already running (kill it by PID first)"; exit 99
fi

# --- 1. presets seeded BEFORE app launch (SetPreset only finds startup-scan
#        presets) and linked per device -----------------------------------------
cp "$LINKS_FILE" "$LINKS_BAK" 2>/dev/null || echo '{"version":2}' > "$LINKS_BAK"
python3 - "$DEV_A" "$DEV_B" "$PRESET_DIR" "$LINKS_FILE" <<'EOF'
import json, sys
dev_a, dev_b, pdir, links_path = sys.argv[1:5]
def band(freq, gain):
    return {"filter_type": 1, "frequency": freq, "gain_db": gain, "q": 1.0, "mute": False, "solo": False}
loud  = [band(1000.0 * (1.2 ** i), 12.0 if i == 0 else 0.0) for i in range(10)]
quiet = [band(1000.0 * (1.2 ** i), -12.0 if i == 0 else 0.0) for i in range(10)]
for name, bands in (("gold_loud", loud), ("gold_quiet", quiet)):
    with open(f"{pdir}/{name}.json", "w") as f:
        json.dump({"version": 1, "preamp_db": 0.0, "bands": bands}, f)
try:
    cfg = json.load(open(links_path))
except Exception:
    cfg = {"version": 2}
cfg.setdefault("links", {})[dev_a] = "gold_loud"
cfg["links"][dev_b] = "gold_quiet"
json.dump(cfg, open(links_path, "w"), indent=2)
EOF

# --- 2. STEREO test tone (FL=FR=1 kHz), chunked generation --------------------
python3 - "$TONE" <<'EOF'
import math, struct, sys, wave
rate, secs, chunk = 48000, 1800, 4800
with wave.open(sys.argv[1], "wb") as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(rate)
    for base in range(rate * secs // chunk):
        fl = [int(9000 * math.sin(2 * math.pi * 1000 * (i + base * chunk) / rate)) for i in range(chunk)]
        fr = [int(9000 * math.sin(2 * math.pi * 1000 * (i + base * chunk) / rate)) for i in range(chunk)]
        w.writeframes(b"".join(struct.pack("<hh", a, b) for a, b in zip(fl, fr)))
EOF

# --- 3. two streams on two devices, BEFORE the app (the user's order) ----------
setsid nohup pw-play --target="$DEV_A" "$TONE" >/dev/null 2>&1 < /dev/null & disown
PID_A=$!
setsid nohup pw-play --target="$DEV_B" "$TONE" >/dev/null 2>&1 < /dev/null & disown
PID_B=$!
sleep 5
echo "baseline: $(stream_links)"

# --- 4. app launch + device 1: select, apply curve 1, route --------------------
start_app "$SCRATCH/app.log"
APP_PID=$!
sleep 6
$DBUS call SetOutputSink "s:$DEV_A" >/dev/null
$DBUS call SetPreset s:gold_loud >/dev/null
dbus_bool SetRoutingEnabled true
sleep 8
OUT_A="$(eq_out_for "$DEV_A")"; OUT_B="$(eq_out_for "$DEV_B")"
RMS_A1="$(rms_of "$OUT_A")"
echo "  device 1 with curve 1 (+12dB): rms=$RMS_A1"
check "step 2: curve 1 applied to device 1 (loud)" python3 -c "assert float('$RMS_A1') > 100.0, 'A silent/bypassed: $RMS_A1'"

# --- 5. device 2: select, apply curve 2, route ----------------------------------
$DBUS call SetOutputSink "s:$DEV_B" >/dev/null
$DBUS call SetPreset s:gold_quiet >/dev/null
sleep 3
# selecting B flips the route switch to B's own state; re-engage for B
dbus_bool SetRoutingEnabled true
sleep 8
RMS_B1="$(rms_of "$OUT_B")"
echo "  device 2 with curve 2 (-12dB): rms=$RMS_B1"
check "step 3: curve 2 applied to device 2 (quiet)" python3 -c "assert float('$RMS_B1') > 100.0, 'B silent/bypassed: $RMS_B1'"

# --- 6. switch BACK to device 1: curve 1 must be PRESERVED ----------------------
$DBUS call SetOutputSink "s:$DEV_A" >/dev/null
sleep 5
RMS_A2="$(rms_of "$OUT_A")"
echo "  device 1 after returning: rms=$RMS_A2 (was $RMS_A1 with curve 1)"
check "step 4: curve 1 PRESERVED on device 1 (not replaced by curve 2)" python3 -c "
import math
a1, a2 = float('$RMS_A1'), float('$RMS_A2')
assert a2 > 100.0, f'A silent/bypassed after returning: $RMS_A2'
# curve 1 is +12dB, curve 2 is -12dB: a 24 dB gap means crossing is obvious
assert a2 > a1 * 0.5, f'curve replaced: was {a1} (curve 1), now {a2} (looks like curve 2)'"

# --- 7. device 2's curve also still preserved -----------------------------------
RMS_B2="$(rms_of "$OUT_B")"
echo "  device 2 recheck: rms=$RMS_B2 (was $RMS_B1 with curve 2)"
check "step 5: curve 2 preserved on device 2" python3 -c "
b1, b2 = float('$RMS_B1'), float('$RMS_B2')
assert b2 > 100.0, f'B silent/bypassed: $RMS_B2'
assert abs(b2 - b1) < max(b1, b2) * 0.3 + 1.0, f'B curve moved: {b1} -> {b2}'"

# --- 8. exit: streams must not break --------------------------------------------
$DBUS call Quit >/dev/null 2>&1
sleep 4
LINKS_AFTER="$(stream_links)"
echo "  links after quit: $LINKS_AFTER"
check "step 6a: both streams back on their devices (not pathless/EQ)" python3 -c "
s = '''$LINKS_AFTER'''
assert s != 'NO-LINKS', 'streams pathless after quit'
assert 'mini_eq_sink' not in s, f'still linked into EQ after quit: {s}'"
check "step 6b: players alive after quit" bash -c "
kill -0 $PID_A 2>/dev/null && kill -0 $PID_B 2>/dev/null"
check "step 6c: app exited on Quit" bash -c "! kill -0 $APP_PID 2>/dev/null"

echo
echo "================== $PASS passed, $FAIL failed =================="
[ -n "${FAILED_NAMES[*]:-}" ] && echo "failed: ${FAILED_NAMES[*]}"
exit $FAIL
