# Plan: microphone processing (beyond upstream)

Status: **planned, not started.** This is deliberately *outside* upstream parity.
`mini-eq` (Python, `upstream/main`) is an output-only tool: it builds one
filter-chain, feeds it the system output sink, and monitors that sink. There is
no capture/record path anywhere in it, so nothing here has an upstream
behaviour to copy. Treat this as new design work, not a port.

## Why

Affects the same reason the output side is worth having: per-device settings.
Today the EQ is a property of the *output*, so two outputs cannot have two
curves, and nothing can be done to what goes *into* the machine. Two things fall
out of that:

1. **Input conditioning.** A microphone that is too quiet forces a huge gain
   boost in the recorder, and whatever is added on the way in is recorded too.
   An EQ on the capture side fixes the signal before it is stored.
2. **A separate monitor path for the mic.** The common live-monitoring problem:
   you want to hear the mic through the EQ, with the EQ, without the latency and
   risk of feeding the processed signal back into the recording path. That
   means a *second* chain, sourced from the mic and played to a monitor device
   the user chooses, independent of the output chain.

## What exists to build on

- `filter_chain.rs` already builds a PipeWire `filter-chain` from an ordered
  port list, and the output chain uses `a->b` (sink to sink) with an internal
  output node named `{VIRTUAL_SINK_BASE}{FILTER_OUTPUT_SUFFIX}`. The same
  builder takes capture ports; the work is in the wiring, not the DSP.
- `routing.rs` already enumerates `Audio/Sink` nodes and speaks the `default`
  metadata. The capture side needs the `Stream/Input/Audio` equivalent, and a
  separate routing table for source streams.
- The blocklist and foreign-target logic just ported for output applies verbatim
  to sources: never touch `speech-dispatcher*` or anything already pointed
  elsewhere, honour `node.dont-move`.
- `analyzer.rs` captures from monitor ports of a sink. A source-side meter needs
  `monitor_FL/FR` ports on a *source* node, which exists on ALSA capture
  devices, and not on every backend.

## Shape of the work

1. **Capture chain, off by default.** A second filter-chain instance sourced
   from a chosen `Stream/Input/Audio` or `Audio/Source`, with its own band state
   and its own preamp. Separate from the output chain: separate module, separate
   coefficient push path, separate reload debounce. Sharing the state struct is
   fine; sharing the module handle is not.
2. **Per-mic settings, keyed the way the output side is being keyed.** The output
   side is moving to per-sink identity (see
   `output_preset_key_for_sink`); the capture side needs the same shape keyed by
   capture device, so plugging in a different interface gets its own curve.
   This is the main argument for doing the per-output preset work first: it
   forces a real identity key rather than one global preset.
3. **Monitor path as a separate, optional stage.** Capture chain output goes to a
   user-chosen monitor sink *by explicit route*, never by default-sink
   inheritance, and never into the recording stream. Needs an explicit "monitor
   through the output EQ" option too, for the case where the user wants to hear
   the mic as the speakers will sound.
4. **Latency and feedback rules, decided before code.** A capture chain adds
   latency to the recorded file. A monitor path can create a feedback loop if it
   ever reaches a microphone. Both need explicit handling and a way to see the
   numbers, not a default that happens to work on this machine.
5. **Metering.** A source-side spectrum, which is a different capture
   (source monitor ports, not sink monitor ports) and a different UI placement.

## Open questions to settle first

- Does the chain stay up when the mic is idle, or start with the first stream?
  Starting with the stream costs a startup gap per session; staying up costs
  CPU and a permanently held node.
- Should the capture chain apply when the app is only monitoring (no recording
  software running), or only when a capture stream exists? The former is
  predictable; the latter avoids surprising latency when nothing is recording.
- Per-app or per-device capture EQ? Per-app needs the same foreign-target
  discipline as the output side; per-device is simpler and matches the output
  side's model.
- What is the safe default for the monitor path? "Off" is the only answer that
  cannot surprise anyone, and it means the feature is opt-in twice.

## Explicitly not in scope

- Changing what is recorded. The capture chain is for conditioning the signal;
  anything that alters the stored audio beyond an EQ is a recorder's job.
- Automatic routing of *sources* system-wide. Hijacking every input on the
  machine is not something to ship by default, and the output side's blocklist
  discipline exists precisely because that went wrong before.

## Also relevant

- `docs/PLAN/output-modes-2026-10-05.md` designs the output-side mode split
  (Selected vs All outputs), per-device curves and monitor selection. Its
  "explicitly out of scope" section explains why multiple simultaneous chains are
  a much larger problem — the same one this plan runs into on the capture side:
  one chain equalises one signal path at a time.

## Related

- Per-output preset switching (the prerequisite this plan leans on) is
  implemented in `core.rs` (`output_preset_key_for_sink`,
  `output_preset_for_sink`) and `window.rs`
  (`apply_output_preset_for_sink`). The same identity key is what the capture
  side needs.
- `docs/PLAN/graph-parity-2026-10-04.md` covers the graph layer, which the
  capture-side meter would also want.
- Upstream has no capture path, so there is nothing to compare against: this
  work will be reviewed on its own merits, not against `upstream/main`.
