# Plan: output modes, per-device curves, and monitor selection

Status: **design agreed, not started.** Replaces the single "System-wide EQ"
switch with an explicit mode choice, and rebuilds the Output page around it.

Decisions taken with the user (2026-10-05):

- **Selected** output mode is the default. **All outputs** (global) is an
  explicit opt-in.
- Write the plan first, implement after review.

This diverges from upstream on purpose. Upstream has one mode
(`SystemWideEqController`) plus a follow-default option on its output dropdown
(`window.py:967`), so there is no upstream behaviour to copy for the mode split.
The routing machinery underneath is already upstream-faithful and stays that way.

## The problem

Two independent decisions are being expressed by one switch and one dropdown:

| Axis | Controlled by | What it does |
|---|---|---|
| Which streams are fed into the EQ | `route_switch` → `RoutingEngine::auto_route_to_sink` | pulls in every eligible stream, whatever device it was on |
| Where the chain plays out | the Output dropdown → the chain's destination | moves `mini_eq_sink_output` only; touches no stream |

With the switch off, the first axis does nothing, so the second has no audible
effect: the dropdown looks broken. That is the whole confusion, and it is why
"Link to Output" could never mean anything coherent — there is no stable notion of
"this device is the one being processed".

One constraint shapes every option below: **there is one filter chain with one
output.** Anything routed into it comes out of the one device it targets, so
"equalise everything" also means "everything now plays from that device". The UI
has to say so rather than let it be discovered.

## Mode semantics

### Selected (default)

Only streams whose current target is the chosen device are routed into the EQ.

- Nothing moves that was not already going to that device. No surprise, and the
  upstream rule that a stream deliberately pointed elsewhere is never touched
  stays absolute.
- The device is the scope of the EQ, which makes the per-device curve
  (below) obviously correct rather than a convenience.
- Cost: apps the user has pointed at that device elsewhere are not covered. That
  is deliberate, and it is the user's mixer that decides.

Predicate: a stream is in scope when its `target.object` matches the chosen
sink's `object.serial`, **or** it has no explicit target at all (WirePlumber is
choosing the default for it). Both forms are already readable from the target
cache (`RoutingEngine::stream_target`), so this is a filter on data we already
have, not new machinery.

### All outputs (opt-in)

Every eligible stream is routed in, exactly as today: upstream's routability
filter (internal/blocklisted/foreign/dont-move) and nothing else.

- **This mode is an explicit opt-out from the foreign-target rule.** Say so in
  the tooltip and in the status line, because with it on, a stream the user
  pointed at device B will be pulled to device A. That is the single most
  surprising thing this app can do, and it must never be the default.

## Output dropdown

- Entry 0 stays **"Default Output (follow system)"** and now genuinely follows:
  the 500 ms watcher retargets the chain when the system default changes. This
  was impossible before, because every device change rebuilt the chain; it is
  now a live move (`retarget_output` → `move_stream_to_target_checked`), which
  is cheap and gapless.
- A specific device pins the chain to it.
- Hotplug keeps the selection by device name (already done in
  `refresh_output_sinks`).

## Per-device curves

Replaces "Link to Output", whose name and behaviour were both wrong.

- **"Save curve for \<device\>"** writes the current curve to that device's link
  in `output-presets.json` (`set_output_preset_link`, keyed by
  `output_preset_key_for_sink`).
- **Auto-load on switch** already exists (`apply_output_preset_for_sink`).
- **Status per device**, using upstream's vocabulary from
  `OUTPUT_PRESET_STATUS_LABELS`: `Applied` / `Different` / `Linked` /
  `Missing` / `Modified`. The two value labels in the current row are dead --
  written only by their own click handler -- so this also fixes what the user
  can actually see.
- Optional **auto-save on switch** preference, off by default: saving without
  being asked is how a curve gets lost.
- Keep **Set Fallback** as the single global default for devices with no link.

Migration: the live config holds `{"links": {"default": "preset_1"}}` from the
old button. On first read, treat a `default` key as the fallback only, never as a
device (already asserted by
`per_sink_preset_lookup_prefers_the_sink_then_the_fallback`), and drop it on the
next write.

## Monitor device

Today the monitor follows the chain's output (`resolve_monitor_target` →
`engine_sink`), so you cannot equalise on headphones and watch the speakers.

- A **Monitor device** dropdown: `Follow EQ output` (default, current
  behaviour) or a specific device.
- Cheap: `retarget_monitor` is already a live retarget.
- Risk: ambiguity about whether monitoring changes where audio goes. Label it
  listen-only, in the tooltip and the status line.

## Output page layout

```
Output mode      [ Selected output ] [ All outputs ]
Device           [ Default Output (follow system) ▾ ]
Curve            <preset name>   [Save for this device]   status: Applied
Monitor          [ Follow EQ output ▾ ]
```

The mode buttons replace the switch, because the mode changes what the rest of
the page means; a switch whose label does not change with its meaning is what
caused the confusion.

## D-Bus

- `GetState`: add `output_mode` (`selected` | `all`), `monitor_sink`, and
  `output_preset` (the preset for the current device) next to the existing
  `preset_name` and `output_sink`.
- `SetRoutingEnabled` keeps its meaning ("are streams routed through the EQ at
  all") and is orthogonal to the mode; a new `SetOutputMode` carries the mode.
- `capabilities`: keep `output-presets`, add `output-mode` and `monitor-sink`.

## Persistence

`~/.config/mini-eq/output-presets.json` gains, alongside `links` and `default`:

```json
{ "links": {...}, "default": "...", "mode": "selected", "monitor": "follow",
  "version": 2 }
```

Version bump with a read path that tolerates `version: 1` (no mode → Selected).

## Sequencing

1. **Mode split.** `RoutingEngine` gains a mode and the scope predicate;
   `auto_route_to_sink` takes the scope. Replace the switch with the two buttons.
2. **Working follow-default.** Watcher retargets the chain live. Removes the
   "the dropdown does nothing" impression even before the mode is chosen.
3. **Per-device curves.** Save button, status labels, de-duplicate the row text,
   drop the dead labels.
4. **Monitor device.**
5. Only then reconsider anything about per-app control (see below).

## Explicitly out of scope

- **A list of sources with a dropdown each.** That is a session manager. It would
  duplicate WirePlumber/pavucontrol, and it would write `target.object` for
  streams the user configured elsewhere — the hijacking that `ba49f88` just
  removed, reintroduced through the front door. With one chain it would also give
  the illusion of per-source EQ, which is not achievable: only one device can be
  equalised at a time. If per-app targeting is wanted, add an "Open in
  pavucontrol" action instead.
- Multiple chains, one per device. That is what would make per-device EQ
  simultaneous rather than per-selected-device, and it is a much larger piece of
  work (see `docs/PLAN/mic-processing-2026-10-05.md` for the same shape of
  problem on the capture side).

## Tests

- Scope predicate: stream with no target → in scope for the default device; with
  the chosen serial → in scope; with another serial → out; blocklisted/internal →
  out regardless of mode.
- Mode round trip: All outputs routes a stream that Selected leaves alone.
- Follow-default: a default-sink change moves the chain without a rebuild (assert
  the live path, not the log line).
- Curve status: Applied / Modified / Missing per device; save writes the key the
  reader will use.

## Open questions

- Should changing mode while streams are routed re-route immediately, or only on
  the next device change? Re-routing on the spot is more predictable; leaving it
  until the next change is less churn. Default: re-route immediately, since the
  user just told us what they want.
- Should "All outputs" remember itself across restarts? Recommend yes (it is in
  the config), with the status line making it obvious.
- Does the monitor device need to survive a chain rebuild? Recommend yes, with
  `Follow EQ output` re-resolving after it.