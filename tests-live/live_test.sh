#!/usr/bin/env bash
# Live end-to-end test for mini-eq RR.
#
# Drives the real app (under Xvfb, no visible window) over D-Bus and asserts
# on the real PipeWire graph via pw-dump. Covers the reported regressions:
#   1. EQ switch Off must hand streams back (unroute), not leave them dangling.
#   2. Startup output_sink must be a real device, and the monitor must tap it.
#   3. Curve model lists the full preset library (unit test; see
#      window_presets.rs::tests::curve_model_always_has_none_and_builtins).
#   4. Device switches must keep the monitor alive and the EQ on the new device.
#   5. Quitting must restore streams (no silence-after-exit).
#
# Usage: ./tests-live/live_test.sh [--keep-old-config-backup]
# Exit code: number of failed assertions (0 = all green).
#
# NOTE: this briefly routes live playback streams through the EQ and back.
# It restores everything (routing, output mode, monitor state) on exit,
# including on failure (trap).

set -u
cd "$(dirname "$0")/.." || exit 1

# Hermetic session bus: a second app instance (manual testing in parallel)
# can steal the well-known name (REPLACE flag), which once made GetState
# answer from the wrong instance mid-run. Re-exec under a private bus unless
# told otherwise (LIVE_TEST_HERMETIC=0).
if [ "${LIVE_TEST_HERMETIC:-1}" = 1 ] && [ -z "${LIVE_TEST_INNER:-}" ]; then
  command -v dbus-run-session >/dev/null 2>&1 || { echo "missing tool: dbus-run-session"; exit 99; }
  exec dbus-run-session -- env LIVE_TEST_INNER=1 "$0" "$@"
fi

APP=./target/release/mini-eq-rr
BUS=io.github.mrproject72.mini_eq_rr
DBUS="python3 tests-live/dbus.py"
PW="python3 tests-live/pw_state.py"
DISPLAY_NUM=:99
WAV=/tmp/mini-eq-live-test-tone.wav
TONE_PID=""
APP_PID=""
XVFB_PID=""
PASS=0
FAIL=0
FAILED_NAMES=()

if [ ! -x "$APP" ]; then
  echo "building release binary first..."
  if ! cargo build --release; then echo "BUILD FAILED"; exit 99; fi
fi

for tool in Xvfb pw-play pw-dump gdbus python3; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing tool: $tool"; exit 99; }
done

cleanup() {
  # Restore persisted settings we may have flipped, then quit the app.
  if kill -0 "$APP_PID" 2>/dev/null; then
    if [ -n "${INIT_ANALYZER:-}" ]; then
      if [ "$INIT_ANALYZER" = "true" ]; then
        $DBUS call SetMonitorEnabled b:true >/dev/null 2>&1
      else
        $DBUS call SetMonitorEnabled b:false >/dev/null 2>&1
      fi
    fi
    if [ -n "${INIT_MODE:-}" ]; then $DBUS call SetOutputMode "s:$INIT_MODE" >/dev/null 2>&1; fi
    if [ -n "${INIT_MON_SINK:-__unset}" ] && [ "$INIT_MON_SINK" != "__unset" ]; then
      $DBUS call SetMonitorSink "s:$INIT_MON_SINK" >/dev/null 2>&1
    fi
    $DBUS call Quit >/dev/null 2>&1
    sleep 2
    kill -0 "$APP_PID" 2>/dev/null && kill "$APP_PID" 2>/dev/null
  fi
  [ -n "$TONE_PID" ] && kill "$TONE_PID" 2>/dev/null
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
  rm -f "$WAV"
  # Restore the user's preset links + drop seeded test presets.
  # Defaults guard early aborts (set -u) before T6 assigns these.
  if [ -f "${LINKS_BAK:-}" ]; then
    cp "$LINKS_BAK" "${LINKS_FILE:-/dev/null}" 2>/dev/null
    rm -f "$LINKS_BAK"
  fi
  if [ -n "${PRESET_DIR:-}" ]; then
    rm -f "$PRESET_DIR/livetest_A.json" "$PRESET_DIR/livetest_B.json" 2>/dev/null
  fi
}
trap cleanup EXIT

check() { # check <name> <command...>
  local name="$1"; shift
  local out
  if out="$("$@" 2>&1)"; then
    PASS=$((PASS+1)); echo "PASS: $name"
  else
    FAIL=$((FAIL+1)); FAILED_NAMES+=("$name"); echo "FAIL: $name"
    [ -n "$out" ] && echo "$out" | sed 's/^/  -- /'
    echo "  -- app alive: $(kill -0 "$APP_PID" 2>/dev/null && echo yes || echo NO)"
  fi
}

getstate() { $DBUS getstate 2>/dev/null; }
state_val() { getstate | grep "^$1=" | cut -d= -f2-; }
# The selected device's EQ sink name, mirroring core::eq_virtual_sink_for.
eq_name_for() {
  python3 -c "
import re, sys
s = re.sub(r'[^A-Za-z0-9_.-]', '_', sys.argv[1]).strip('_.-')
print('mini_eq_sink_' + (s if s else 'unknown'))" "$(state_val output_sink)"
}
pwstate() { PW_STATE_DEFAULT_EQ="$(eq_name_for)" $PW; }

wait_for() { # wait_for <secs> <name> <bash-condition-using-$S-and-$P>
  local secs="$1" name="$2"; shift 2
  local i
  for ((i=0; i<secs*2; i++)); do
    S="$(getstate)" P="$(pwstate)"
    if eval "$1" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  echo "  -- wait_for '$name' timed out. Last GetState routed=$(echo "$S" | grep '^routed='), mode=$(echo "$S" | grep '^output_mode='), sink=$(echo "$S" | grep '^output_sink='), analyzer=$(echo "$S" | grep '^analyzer_enabled=')" >&2
  echo "  -- last pw: $(echo "$P" | python3 -c "
import json,sys
try:
    p = json.loads(sys.stdin.read())
    m = p.get('minieq')
    print('minieq=' + (m['serial'] if m else 'NONE'), 'links=' + str(len(p.get('monitor_links', []))))
except Exception as e:
    print('pw-dump FAILED:', e)
")" >&2
  return 1
}

# --- 0. precondition: no instance running -----------------------------------
if gdbus call --session --dest "$BUS" --object-path /io/github/mrproject72/mini_eq_rr/Control \
     --method "$BUS".MiniEqRR.Control.GetState >/dev/null 2>&1; then
  echo "ABORT: a mini-eq instance is already running (bus name owned). Close it first."
  exit 99
fi

# --- 1. test tone ------------------------------------------------------------
python3 - "$WAV" <<'EOF'
import math, struct, sys, wave
path = sys.argv[1]
rate, secs = 48000, 300  # 300 s 1000 Hz tone (outlives the test run)
# 1000 Hz: matches livetest_A's boosted band, so the wet/bypass level
# comparison in T3b measures the actual EQ curve, not room mix.
with wave.open(path, "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(rate)
    chunk = 48000
    for start in range(0, rate * secs, chunk):
        frames = b"".join(
            struct.pack("<h", int(12000 * math.sin(2 * math.pi * 1000 * i / rate)))
            for i in range(start, min(start + chunk, rate * secs))
        )
        w.writeframes(frames)
EOF

BEFORE_STREAMS="$(pwstate | python3 -c 'import json,sys; print(" ".join(str(s["id"]) for s in json.load(sys.stdin)["streams"]))')"
# EQ sink serials owned by anyone else (e.g. a manually tested instance):
# the quit check must only assert that THIS run leaked nothing.
EQ_SERIALS_BEFORE="$(pwstate | python3 -c 'import json,sys; print(" ".join(v["serial"] for v in json.load(sys.stdin)["eq_sinks"].values()))')"

# --- 2. launch app under Xvfb -------------------------------------------------
Xvfb $DISPLAY_NUM >/tmp/mini-eq-live-xvfb.log 2>&1 &
XVFB_PID=$!
sleep 1
DISPLAY=$DISPLAY_NUM RUST_LOG=${APP_LOG_LEVEL:-} MINI_EQ_DEBUG_ROUTING=${APP_DEBUG_ROUTING:-} "$APP" >/tmp/mini-eq-live-app.log 2>&1 &
APP_PID=$!

echo -n "waiting for D-Bus service..."
ok=0
for _ in $(seq 1 60); do
  if getstate | grep -q "^output_sink="; then ok=1; break; fi
  sleep 0.5
done
[ "$ok" = 1 ] || { echo " app never appeared on D-Bus. log tail:"; tail -20 /tmp/mini-eq-live-app.log; exit 99; }
echo " up."

INIT_MODE="$(state_val output_mode)"
INIT_MON_SINK="$(state_val monitor_sink)"
INIT_ANALYZER="$(state_val analyzer_enabled)"
INIT_SINK="$(state_val output_sink)"

# --- 3. start tone, identify its stream ----------------------------------------
# Explicit --target: WirePlumber remembers per-app routes across runs and
# would otherwise restore an old target (observed: headset serial from a
# previous run), making scope assertions nondeterministic. Pinning the tone
# to the startup sink mirrors a user playing on their default device.
pw-play --target="$INIT_SINK" "$WAV" >/tmp/mini-eq-live-tone.log 2>&1 &
TONE_PID=$!
sleep 3
TONE_ID="$(pwstate | python3 -c "
import json,sys
before = set('$BEFORE_STREAMS'.split())
for s in json.load(sys.stdin)['streams']:
    if str(s['id']) not in before and ('pw-play' in s['name'] or 'pw-play' in s['app']):
        print(s['id']); break
")"
[ -n "$TONE_ID" ] || { echo "ABORT: tone stream not found"; pwstate | python3 -m json.tool | head -40; exit 99; }
echo "tone stream id: $TONE_ID"

stream_target_object() {
  pwstate | python3 -c "
import json,sys
for s in json.load(sys.stdin)['streams']:
    if str(s['id']) == '$TONE_ID':
        print(s['target_object']); break
"
}

# --- T1: startup state ----------------------------------------------------------
# Abort (not just fail) when the critical values are empty: every later
# assertion would compare empty==empty and pass vacuously.
echo "--- T1: startup state"
[ -n "$INIT_SINK" ] || { echo "ABORT: output_sink empty at startup (engine did not start)"; exit 99; }
MINIEQ_SERIAL="$(pwstate | python3 -c 'import json,sys; m=json.load(sys.stdin)["minieq"]; print(m["serial"] if m else "")')"
[ -n "$MINIEQ_SERIAL" ] || { echo "ABORT: mini_eq_sink missing at startup"; exit 99; }
check "startup routed=false" test "$(state_val routed)" = "false"
SINKS_JSON="$(pwstate | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin)["sinks"].keys()))')"
check "startup output_sink is a real sink (not hardcoded)" bash -c "echo \" $SINKS_JSON \" | grep -q \" $INIT_SINK \""
echo "  output_sink=$INIT_SINK"

# pick a second sink for device-switch tests (prefer a real ALSA sink over
# virtual ones like multi_out, which may have no monitor ports)
SINK_B="$(pwstate | python3 -c "
import json,sys
sinks = list(json.load(sys.stdin)['sinks'].keys())
cands = [s for s in sinks if s != '$INIT_SINK']
alsa = [s for s in cands if s.startswith('alsa_')]
print((alsa or cands)[0] if (alsa or cands) else '')
")"
echo "  second sink: ${SINK_B:-(none available)}"

# --- Per-output preset seeding -------------------------------------------------
# Two distinguishable presets, each linked to one output. Backs up the user's
# links file and preset dir additions; cleanup() restores both. Defined early
# so the audibility check (T3b) can use livetest_A before T6 re-seeds.
# Real preset storage is ~/.config/mini-eq/output (preset_storage_dir);
# ~/.config/mini-eq/presets is stale legacy and invisible to the app.
PRESET_DIR="$HOME/.config/mini-eq/output"
LINKS_FILE="$HOME/.config/mini-eq/output-presets.json"
LINKS_BAK=/tmp/mini-eq-live-links.bak.json
seed_output_presets() {
  # Back up once: this runs twice per run (T3b + T6) and the second backup
  # must not capture the first seeding, or cleanup restores seeded links.
  if [ ! -f "$LINKS_BAK" ]; then
    cp "$LINKS_FILE" "$LINKS_BAK" 2>/dev/null || echo '{"version":2}' > "$LINKS_BAK"
  fi
  python3 - "$INIT_SINK" "$SINK_B" "$PRESET_DIR" <<'EOF'
import json, sys
sink_a, sink_b, pdir = sys.argv[1], sys.argv[2], sys.argv[3]
def band(freq, gain, ftype=1):
    return {"filter_type": ftype, "frequency": freq, "gain_db": gain,
            "q": 1.0, "mute": False, "solo": False}
bands_a = [band(1000.0 * (1.2 ** i), 12.0 if i == 0 else 0.0) for i in range(10)]
bands_b = [band(1000.0 * (1.2 ** i), -6.0 if i == 0 else 0.0) for i in range(10)]
for name, bands in (("livetest_A", bands_a), ("livetest_B", bands_b)):
    with open(f"{pdir}/{name}.json", "w") as f:
        json.dump({"version": 1, "preamp_db": 0.0, "bands": bands}, f)
print("seeded")
EOF
  python3 - "$LINKS_FILE" "$INIT_SINK" "$SINK_B" <<'EOF'
import json, sys
path, sink_a, sink_b = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    cfg = json.load(open(path))
except Exception:
    cfg = {"version": 2}
cfg.setdefault("links", {})[sink_a] = "livetest_A"
if sink_b:
    cfg["links"][sink_b] = "livetest_B"
json.dump(cfg, open(path, "w"), indent=2)
print("linked")
EOF
}

# RMS of a few seconds captured straight from a device EQ's OUTPUT node.
# Unlike the analyzer (which taps the room mix incl. the user's own music),
# this carries only what this run routed -- deterministic by construction.
eq_output_rms() {
  local eq_out="$1" f=/tmp/mini-eq-live-lv.raw
  rm -f "$f"
  timeout 8 pw-record --target="$eq_out" "$f" >/dev/null 2>&1
  [ -s "$f" ] || return 1
  python3 -c "
import struct, math, sys
d = open('$f', 'rb').read()
s = struct.unpack('<%dh' % (len(d) // 2), d)
print('%.1f' % math.sqrt(sum(x * x for x in s) / len(s)))
"
}

# Max analyzer bin, or empty when no emission arrives.
max_level() {
  local levels
  levels="$(read_levels)"
  [ -z "$levels" ] && return 1
  python3 -c "print(max(map(float, '''$levels'''.split())))"
}

# Block until the analyzer reports a live (non-floor) spectrum, so level
# comparisons never race capture negotiation (which can take a while after
# a monitor start/retarget on a loaded box).
wait_for_levels() {
  local i got
  for i in $(seq 1 12); do
    got="$(read_levels)" || got=""
    if [ -n "$got" ] && python3 -c "import sys; sys.exit(0 if max(map(float, '''$got'''.split())) > 0.01 else 1)"; then
      return 0
    fi
    sleep 5
  done
  return 1
}

# The tone's current target serial (empty when WP moved it or it died).
tone_target_now() {
  pwstate | python3 -c "
import json,sys
for s in json.load(sys.stdin)['streams']:
    if str(s['id']) == '$TONE_ID':
        print(s['target_object']); break
"
}

# True while the tone node is actively running with data (present and
# targeted is not enough -- a stalled pw-play looks identical downstream).
tone_flowing() {
  pw-top -b -n 3 2>/dev/null | grep -E "^ *R +$TONE_ID +[1-9]" | grep -q .
}

# Re-assert the tone is routed into the given eq serial (WirePlumber
# sometimes re-homes streams mid-test); re-route once if it strayed.
ensure_tone_routed() {
  local want="$1"
  if [ "$(tone_target_now)" != "$want" ]; then
    $DBUS call SetRoutingEnabled b:false >/dev/null
    sleep 2
    $DBUS call SetRoutingEnabled b:true >/dev/null
    sleep 3
  fi
  test "$(tone_target_now)" = "$want"
}

# --- T2: switch on --------------------------------------------------------------
echo "--- T2: EQ on routes the tone through mini_eq_sink"
$DBUS call SetRoutingEnabled b:true >/dev/null
check "routed=true after on" wait_for 15 routed-true \
  '[ "$(echo "$S" | grep "^routed=" | cut -d= -f2)" = "true" ]'
check "tone target is mini_eq_sink" wait_for 15 tone-routed \
  "[ \"\$(echo \"\$P\" | python3 -c \"import json,sys; print([s['target_object'] for s in json.load(sys.stdin)['streams'] if str(s['id'])=='$TONE_ID'][0])\")\" = \"$MINIEQ_SERIAL\" ]"

# --- T3: A/B --------------------------------------------------------------------
echo "--- T3: A/B bypass"
$DBUS call SetEqEnabled b:false >/dev/null
sleep 2
check "eq_enabled=false reported" test "$(state_val eq_enabled)" = "false"
$DBUS call SetEqEnabled b:true >/dev/null
sleep 2
check "eq_enabled=true reported" test "$(state_val eq_enabled)" = "true"

# --- T3b: audibility (wet vs bypass on a boosted curve) -------------------------
# Loads livetest_A (+12 dB at the tone's 1000 Hz) and asserts the measured
# spectrum actually moves between wet and bypass. This is the only check that
# proves the DSP processes audio -- routing checks alone cannot (a dead chain
# with live links looks identical). Guarded by ensure_tone_routed: WP
# sometimes re-homes the tone mid-test, which would fake a difference.
echo "--- T3b: EQ is audible (wet vs bypass levels)"
# Don't restart a running monitor: stop+start tears down the capture and its
# port re-linking outlasts the level-read window, emptying every assertion.
if [ "$(state_val analyzer_enabled)" != "true" ]; then
  $DBUS call SetMonitorEnabled b:true >/dev/null
  sleep 2
fi
seed_output_presets
$DBUS call SetPreset s:livetest_A >/dev/null
sleep 3
EQ_SERIAL="$(pwstate | python3 -c 'import json,sys; m=json.load(sys.stdin)["minieq"]; print(m["serial"] if m else "")')"
check "tone routed for level check" ensure_tone_routed "$EQ_SERIAL"
check "tone actually flowing (not stalled)" tone_flowing
EQ_OUT="$(python3 -c "import re;print('mini_eq_sink_'+re.sub(r'[^A-Za-z0-9_.-]','_','$INIT_SINK').strip('_.-')+'_output')")"
sleep 3
WET_RMS="$(eq_output_rms "$EQ_OUT")" || WET_RMS=""
check "wet chain output audible" python3 -c "assert float('$WET_RMS') > 100.0, 'chain silent while routed: $WET_RMS'"
$DBUS call SetEqEnabled b:false >/dev/null
sleep 3
DRY_RMS="$(eq_output_rms "$EQ_OUT")" || DRY_RMS=""
check "dry chain output audible (bypass passes audio)" python3 -c "assert float('$DRY_RMS') > 100.0, 'bypass silent: $DRY_RMS'"
check "wet hotter than bypass (+12 dB curve)" python3 -c "assert float('$WET_RMS') > float('$DRY_RMS') * 1.5, 'wet=$WET_RMS dry=$DRY_RMS'"
echo "  wet=$WET_RMS dry=$DRY_RMS"
$DBUS call SetEqEnabled b:true >/dev/null
sleep 2

# --- T4: output mode --------------------------------------------------------------
echo "--- T4: output mode round-trip"
$DBUS call SetOutputMode s:reroute >/dev/null
sleep 2
check "mode=reroute reported" test "$(state_val output_mode)" = "reroute"
$DBUS call SetOutputMode s:selected >/dev/null
sleep 2
check "mode=selected reported" test "$(state_val output_mode)" = "selected"

# --- T5/T6: monitor ---------------------------------------------------------------
echo "--- T5: monitor taps the EQ output and hears the tone"
$DBUS call SetMonitorEnabled b:true >/dev/null
check "analyzer_enabled=true" wait_for 15 mon-on \
  '[ "$(echo "$S" | grep "^analyzer_enabled=" | cut -d= -f2)" = "true" ]'
check "monitor linked from EQ output sink" wait_for 15 mon-link \
  "[ \"\$(echo \"\$P\" | python3 -c \"import json,sys; l=json.load(sys.stdin)['monitor_links']; print(len(l))\")\" -ge 1 ]"
MON_FROM="$(pwstate | python3 -c 'import json,sys; l=json.load(sys.stdin)["monitor_links"]; print(l[0]["out_node"] if l else "")')"
check "monitor taps current output sink ($INIT_SINK)" test "$MON_FROM" = "$INIT_SINK"
echo "  monitor out_node=$MON_FROM"
echo -n "waiting for AnalyzerLevelsChanged signal (need 2 emissions)..."
if timeout 15 gdbus monitor --session --dest "$BUS" 2>/dev/null | grep -m2 "AnalyzerLevelsChanged" >/dev/null; then
  PASS=$((PASS+1)); echo " up. PASS: spectrum signal flows while tone plays"
else
  FAIL=$((FAIL+1)); FAILED_NAMES+=("spectrum signal flows"); echo " timeout. FAIL: spectrum signal flows"
fi

# --- T6: sticky chain + per-output presets --------------------------------------
# Setup: two distinguishable presets, each linked to one output. Backs up the
# user's links file and preset dir additions; cleanup() restores both.
if [ -n "$SINK_B" ]; then
  echo "--- T6: per-device EQ independence (Selected)"
  seed_output_presets
  # Our EQ is ON for A at this point (mode selected, tone active).
  $DBUS call SetPreset s:livetest_A >/dev/null
  sleep 2
  check "preset A active on A" test "$(state_val preset_name)" = "livetest_A"
  EQ_A="$(python3 -c "import re;print('mini_eq_sink_'+re.sub(r'[^A-Za-z0-9_.-]','_','$INIT_SINK').strip('_.-'))")"
  EQ_A_SERIAL="$(pwstate | python3 -c "import json,sys; m=json.load(sys.stdin)['eq_sinks'].get('$EQ_A'); print(m['serial'] if m else '')")"
  check "tone sits on eq(A)" test "$(stream_target_object)" = "$EQ_A_SERIAL"

  # Switch selected device to B: A's chain and streams must stay where they are.
  # engine/preset follow the new device; routed flips to B's (empty) state.
  $DBUS call SetOutputSink "s:$SINK_B" >/dev/null
  sleep 2
  check "GetState moved to B" test "$(state_val output_sink)" = "$SINK_B"
  check "A's stream still processing on eq(A)" test "$(stream_target_object)" = "$EQ_A_SERIAL"
  check "preset livetest_B recalled for B" test "$(state_val preset_name)" = "livetest_B"
  check "routed reflects selected B (off)" test "$(state_val routed)" = "false"
  check "output_preset link reads livetest_B" test "$(state_val output_preset)" = "livetest_B"

  # Back to A: everything left exactly as we found it.
  $DBUS call SetOutputSink "s:$INIT_SINK" >/dev/null
  sleep 2
  check "back on A" test "$(state_val output_sink)" = "$INIT_SINK"
  check "preset livetest_A recalled for A" test "$(state_val preset_name)" = "livetest_A"
  check "routed=true again (A's EQ still up)" test "$(state_val routed)" = "true"
  check "tone still on eq(A)" test "$(stream_target_object)" = "$EQ_A_SERIAL"

  # EQ on for B: chain created, B's curve comes from B's preset; A unharmed.
  $DBUS call SetOutputSink "s:$SINK_B" >/dev/null
  sleep 2
  $DBUS call SetRoutingEnabled b:true >/dev/null
  sleep 2
  EQ_B="$(python3 -c "import re;print('mini_eq_sink_'+re.sub(r'[^A-Za-z0-9_.-]','_','$SINK_B').strip('_.-'))")"
  EQ_B_SERIAL="$(pwstate | python3 -c "import json,sys; m=json.load(sys.stdin)['eq_sinks'].get('$EQ_B'); print(m['serial'] if m else '')")"
  check "chain for B exists once its EQ is enabled" test -n "$EQ_B_SERIAL"
  check "tone on A stays on eq(A) while B's EQ comes up" test "$(stream_target_object)" = "$EQ_A_SERIAL"
  # Back to A and leave state consistent for the following checks.
  $DBUS call SetOutputSink "s:$INIT_SINK" >/dev/null
  sleep 2
  check "back on A for teardown" test "$(state_val output_sink)" = "$INIT_SINK"
else
  echo "SKIP: device-switch tests (only one sink)"
fi

# --- T8: switch off restores ----------------------------------------------------------------
echo "--- T7: EQ off hands the tone back"
$DBUS call SetRoutingEnabled b:false >/dev/null
check "tone target no longer mini_eq_sink" wait_for 15 tone-unrouted \
  "[ \"\$(echo \"\$P\" | python3 -c \"import json,sys; print([s['target_object'] for s in json.load(sys.stdin)['streams'] if str(s['id'])=='$TONE_ID'][0])\")\" != \"$MINIEQ_SERIAL\" ]"
TGT_OFF="$(stream_target_object)"
check "tone target valid (empty=WP default, or a real sink serial)" python3 -c "
import json, subprocess, sys
p = json.loads(subprocess.run(['pw-dump'], capture_output=True, text=True, timeout=30).stdout)
serials = {str(o['info']['props'].get('object.serial', '')) for o in p
             if o.get('type', '').endswith('PipeWire:Interface:Node')}
assert '$TGT_OFF' == '' or '$TGT_OFF' in serials, 'dangling target $TGT_OFF'"
check "routed=false after off" test "$(state_val routed)" = "false"
echo "  tone target_object='$TGT_OFF' (minieq was $MINIEQ_SERIAL)"

# on again after device hops (the reported 'loses monitor/EQ' scenario)
$DBUS call SetRoutingEnabled b:true >/dev/null
check "re-on routes again" wait_for 15 re-on \
  "[ \"\$(echo \"\$P\" | python3 -c \"import json,sys; m=json.load(sys.stdin)['minieq']; print(m['serial'] if m else '')\")\" = \"\$(echo \"\$P\" | python3 -c \"import json,sys; print([s['target_object'] for s in json.load(sys.stdin)['streams'] if str(s['id'])=='$TONE_ID'][0])\")\" ]"
check "monitor links survive re-on" wait_for 15 mon-survives \
  "[ \"\$(echo \"\$P\" | python3 -c \"import json,sys; l=json.load(sys.stdin)['monitor_links']; print(len(l))\")\" -ge 1 ]"
$DBUS call SetRoutingEnabled b:false >/dev/null
sleep 2

# --- T9: presets ------------------------------------------------------------------
echo "--- T8: presets"
PRESETS="$($DBUS call ListPresets 2>/dev/null)"
if [ -n "$PRESETS" ] && [ "$PRESETS" != "([] ,)" ]; then
  FIRST="$(echo "$PRESETS" | python3 -c "import sys,re; m=re.findall(r\"'([^']+)'\", sys.stdin.read()); print(m[0] if m else '')")"
  if [ -n "$FIRST" ]; then
    $DBUS call SetPreset "s:$FIRST" >/dev/null
    check "preset loads ($FIRST)" wait_for 15 preset-loads \
      "[ \"\$(echo \"\$S\" | grep '^preset_name=' | cut -d= -f2-)\" = \"$FIRST\" ]"
  fi
else
  echo "SKIP: no custom presets to load (curve-list covered by unit test)"
fi

# --- T10: quit restores ----------------------------------------------------------------
echo "--- T9: quit restores the tone"
MINIEQ_BEFORE="$(pwstate | python3 -c 'import json,sys; m=json.load(sys.stdin)["minieq"]; print(m["serial"] if m else "")')"
$DBUS call Quit >/dev/null 2>&1
for _ in $(seq 1 20); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.5; done
check "app exited on Quit" bash -c "! kill -0 $APP_PID 2>/dev/null"
APP_PID="" # already gone; don't double-kill in cleanup
sleep 2
AFTER="$(pwstate)"
check "this run leaked no EQ chains" python3 -c "
import json,sys
before = set('$EQ_SERIALS_BEFORE'.split())
after = {v['serial'] for v in json.load(sys.stdin)['eq_sinks'].values()}
leaked = after - before
assert not leaked, f'chains left behind: {leaked}'" <<< "$AFTER"
TGT_END="$(echo "$AFTER" | python3 -c "
import json,sys
m = [s['target_object'] for s in json.load(sys.stdin)['streams'] if str(s['id'])=='$TONE_ID']
print(m[0] if m else 'STREAM-GONE')")"
check "tone stream still alive at end" test "$TGT_END" != "STREAM-GONE"
check "tone not left pointing at destroyed sink" test "$TGT_END" != "$MINIEQ_BEFORE"
echo "  tone target_object='$TGT_END' (minieq was $MINIEQ_BEFORE)"

echo
echo "================== $PASS passed, $FAIL failed =================="
if [ "$FAIL" -ne 0 ]; then printf 'failed: %s\n' "${FAILED_NAMES[@]}"; fi
exit "$FAIL"
