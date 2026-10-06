# 2026-10-05 — Step 3 of the output-modes plan: the mode split

`docs/PLAN/output-modes-2026-10-05.md` step 3. The switch was one widget whose
label meant two things, and the scope of the routing was hard-coded: every
eligible stream was moved in, including ones the user had deliberately pointed
at another device.

## The switch is now two orthogonal controls

`System-wide EQ` is still a switch — it is "are streams routed through the EQ at
all". Beside it are two toggle buttons in a linked group, named for what they
do to the streams rather than for a scope:

- **Selected** (default): only streams already aimed at the chosen device are
  routed in. The device you picked is the only thing being processed.
- **Reroute**: every eligible stream is moved in regardless of device. This is
  the explicit opt-out from the foreign-target rule, and it is never the
  default because overriding a stream the user pointed elsewhere is the single
  most surprising thing this app can do.

The mode is read from the buttons at switch-on and persisted, so toggling off
and back on restores the mode rather than always re-routing everything. The
default is Selected, which is also what a missing or version-1 config reads as.

## The scope predicate

`RoutingEngine::target_in_scope(target_object, sink_serial, mode)` is pure —
it takes the target string rather than a stream, so it is testable without a
live PipeWire connection:

- **Selected**: no explicit target → in scope (WirePlumber is choosing the
  default, which is the device we picked); the chosen serial → in scope; any
  other serial → out.
- **Reroute**: everything in scope, including streams with a foreign target.

The blocklist is applied before this predicate, so it is only ever asked about
a stream that survived it. The chosen sink is identified by its
`object.serial`, which is the value WirePlumber matches on in `target.object` —
comparing against the serial rather than the node name is what makes "this
stream is already going to the device I picked" true.

`routable_output_streams()` now takes a mode, and `auto_route_to_sink_with_mode`
routes under an explicit mode rather than always taking everything.

## Config: version 2, mode and monitor persisted

`OutputPresetConfig` gains `mode` and `monitor`; `output-presets.json` is now
version 2. The reader tolerates version 1 by treating a missing `mode` as
`Selected` and a missing `monitor` as follow, and it drops the legacy `"default"`
link key (written by the old Link button) on the next write — that key is the
fallback, never a device, and reading it back as a sink made
`output_preset_for_sink("default")` return a preset.

New: `output_routing_mode` / `set_output_routing_mode` (with path-injectable
variants), `output_monitor_sink` / `set_output_monitor_sink`, plus the
`OutputRoutingMode` enum with `from_mode_str` / `as_str`.

## D-Bus: output_mode, output_preset, monitor_sink

`MiniEqAppHandler` gains `output_mode`, `output_preset`, `monitor_sink`, and
`set_output_mode`; `SetOutputMode` is a new method (`s` argument, the same
`selected`/`reroute` values the UI buttons use); `output_mode`,
`output_preset` and `monitor_sink` are published in `GetState`; the
`output-mode`, `output-preset` and `monitor-sink` capabilities are advertised.
`AppState` publishes these per tick from the window rather than holding stale
values, and `SetOutputMode` mirrors the widget and re-routes when the engine is
on, keeping the two in agreement whichever side moved first.

## Verified

- `cargo test --lib`: 128 passed (was 123), including
  `mode_scope_predicate`, `output_routing_mode_roundtrips`, and the v1/v2
  config migration tests.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo build --release` all clean.
- Live: switching the mode while the engine is on re-routes immediately
  (logged `Output mode -> Reroute (re-routed)`); switching off hands the
  streams back and disables the A/B switch, which is inert in that state.

## Not changed

- The output dropdown and per-device preset linking are step 4.
- Auto-write-back and the preset picker are step 4.
- The monitor-device dropdown is step 5.
- Per-app control is still out of scope.