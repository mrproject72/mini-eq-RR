# Multi-chain EQ: one chain per output device

Status: design agreed 2026-10-06, implementing.
Supersedes: sticky-chain refusal (no longer needed — switching devices never
disrupts anything) and the single `engine_sink` model.

## User spec (confirmed)

Selected mode = one independent EQ context per output device. Each device
owns on/off, curve/preset, and its streams. Selecting a device only changes
which context is shown/edited. Leaving preserves everything; returning
restores everything. Streams are never moved between devices by switching.
"EQ in place" means audibly processing — so each active device needs its own
running chain, simultaneously.

## Design

- **One virtual sink + filter-chain module per physical device.**
  Sink name: `mini_eq_sink_<sanitized device name>` (`core::eq_virtual_sink_for`),
  e.g. `mini_eq_sink_alsa_output_pci_0000_04_00_6_analog_stereo`.
  `VIRTUAL_SINK_BASE` (`mini_eq_sink`) stays the prefix; every
  `starts_with(VIRTUAL_SINK_BASE)` check keeps working.
- **Chains are created on demand** (first route/enable/monitor on that
  device) and live until quit. No retargeting ever: a chain is born already
  pointing at its device. Default-sink change just means new streams resolve
  elsewhere.
- **Backend owns `chains: HashMap<physical_sink, DeviceChain>`**
  (`{ module, filter_node, bands, preamp }`). All band/live-push APIs take a
  device. Single-device behavior must keep working throughout the refactor.
- **Routing routes per effective device**: a stream aimed at device X goes to
  X's virtual sink. Records stay per-stream (origin). `routed`/`has_routed`
  become per-virtual-sink queries. `processing_path_targets` covers ALL our
  sinks. rescope/unroute/suspend take an explicit device scope.
- **Window: the Output dropdown selects the editing context.**
  Faders/curve/preset panel show the selected device; switching devices saves
  faders into the old device and loads the new device's bands (+ its linked
  preset, as today). The header switch toggles the SELECTED device only.
  Monitor taps the selected device (as today).
- **D-Bus keeps its methods, operating on the selected device**
  (`SetOutputSink` now only reselects). Documented in the plan + updates.
- **Per-device on/off state**: a device is "on" while ≥1 stream sits in its
  virtual sink (derived, not tracked — cannot drift).

## Phases

1. (foundation, no behavior change) `core::eq_virtual_sink_for` + sanitize +
   unit tests; parametrize `filter_chain` module-args on sink base;
   `PipeWireBackend` gains the chain map + per-device create/push/bands
   alongside the existing single-chain fields.
2. Routing per-device scope (virtual-sink params, per-sink records queries).
3. Window: per-device fader save/load, header switch per selected device,
   drop sticky refusal (obsolete), monitor unchanged.
4. D-Bus docs/semantics note; live test: two simultaneous tones on two
   devices, independent on/off + curves + presets, free switching, quit
   restores all.
5. Remove single-chain leftovers (`engine_sink`, `chain_output_sink`,
   `current_sink` singletons where superseded).

## Test strategy

- Unit: naming/sanitize; pod encoding (existing); rescope pure parts.
- Live (`tests-live/live_test.sh` rewritten T6): toneA on default + toneB
  pinned to second sink; EQ on A only → B untouched; curve edit on A inaudible
  on B; enable B with other preset; switch dropdown freely mid-playback;
  quit → both restored.
