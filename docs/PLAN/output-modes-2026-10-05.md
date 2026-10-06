# Plan: output modes, per-device curves, and monitor selection

Status: **design agreed, not started.** Replaces the single "System-wide EQ"
switch with an explicit mode choice, and rebuilds the Output page around it.

Decisions taken with the user (2026-10-05):

- The mode buttons are **Selected** and **Reroute**, named for what they do to
  the streams rather than for a scope. **Selected** is the default; **Reroute**
  is an explicit opt-in.
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

Nothing is moved. Only the streams already playing to the chosen device are
routed into the EQ, so the device you picked is the only thing being processed.

- No surprise: nothing moves that was not already going to that device, and the
  upstream rule that a stream deliberately pointed elsewhere is never touched
  stays absolute.
- The device is the scope of the EQ, which makes the per-device curve (below)
  obviously correct rather than a convenience.
- Cost: apps the user has pointed at that device elsewhere are not covered. That
  is deliberate, and it is the user's mixer that decides.

Predicate: a stream is in scope when its `target.object` matches the chosen
sink's `object.serial`, **or** it has no explicit target at all (WirePlumber is
choosing the default for it). Both forms are already readable from the target
cache (`RoutingEngine::stream_target`), so this is a filter on data we already
have, not new machinery.

### Reroute (opt-in)

Every eligible stream is moved into the EQ, whatever device it was playing to:
upstream's routability filter (internal/blocklisted/foreign/dont-move) and
nothing else. This is today's behaviour, under a name that says what it does.

- **This mode is an explicit opt-out from the foreign-target rule.** Say so in
  the tooltip and in the status line, because with it on a stream the user
  pointed at device B gets pulled to device A. Moving audio between devices
  behind the user's back is the single most surprising thing this app can do,
  which is exactly why the button is named for the reroute and is never the
  default.

## Output dropdown

- Entry 0 stays **"Default Output (follow system)"** and now genuinely follows:
  the 500 ms watcher retargets the chain when the system default changes. This
  was impossible before, because every device change rebuilt the chain; it is
  now a live move (`retarget_output` → `move_stream_to_target_checked`), which
  is cheap and gapless.
- A specific device pins the chain to it.
- Hotplug keeps the selection by device name (already done in
  `refresh_output_sinks`).

## Curves: presets are the storage, the link is only a name

Correction to an earlier draft of this plan, from the user: a "Save curve for
<device>" action is redundant. Presets already exist and already store curves,
and a device's curve changing is the normal case, not something to be saved.
So there is no second storage concept here — only a mapping from device to
preset name, plus the two behaviours that were missing around it.

### Automatic, no save step

- A device with a linked preset **loads it when you select that device**
  (`apply_output_preset_for_sink`, already implemented).
- **Edits are written back automatically.** While the selected device has a link,
  changing the curve updates that preset by itself — debounced, not per edit.
  Nothing to press, and the device's curve is simply whatever you last had, which
  is what was asked for.
- **Guard:** only when the linked preset belongs to exactly one device. A preset
  linked from several devices is a starting point the user may want to keep, and
  silently overwriting it from one device would be its own surprise. In that case
  the chip says `Modified` and the user chooses.

### Update preset: the missing primitive

The workaround for updating a filter is: press `+`, then rename the new preset to
the same name as the existing one. That is a hack caused by a missing overwrite
action, and it belongs to the preset panel rather than to this mode work.

- Add an **Update** action next to `+`: write the current curve into the selected
  preset. Enabled exactly when the state chip reads `Modified`.
- It works whatever the curve came from — a linked device, a loaded preset, an
  AutoEq import — so the "+ and rename" dance has a one-click replacement.
- Small and independent. Worth doing before or alongside the mode split rather
  than after it.

### Status

Reuse the preset panel's existing state chip (`Saved` / `Modified` / `Neutral` /
`Unsaved`, `window_presets.rs`) rather than inventing a second status vocabulary
in the Output page. Upstream's `Applied / Different / Linked / Missing /
Modified` is the same idea in a different place; one source of truth is better
than two that can disagree.

### Keeping "Link to Output"

The link still has a job — it says *which* preset a device uses — but it should
not read as a save. Renamed to a preset picker plus an unlink:

```
Curve            [ <preset> ▾ ]  [unlink, when linked]
```

Choosing a preset in that dropdown links it to the current device immediately.
That replaces both dead value labels (`fallback_label`, `link_label` are written
only by their own click handlers, so they always read `None`) and the row that
currently says "Link to Output" twice.

**Set Fallback** stays as it is: one global preset for devices with no link.

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
Output mode      [ Selected ]  [ Reroute ]
Device           [ Default Output (follow system) ▾ ]
Curve            [ <preset> ▾ ]   [unlink]     chip: Saved / Modified
Monitor          [ Follow EQ output ▾ ]
```

The mode buttons replace the switch, because the mode changes what the rest of
the page means; a switch whose label does not change with its meaning is what
caused the confusion.

## D-Bus

- `GetState`: add `output_mode` (`selected` | `reroute`), `monitor_sink`, and
  `output_preset` (the preset for the current device) next to the existing
  `preset_name` and `output_sink`.
- `SetRoutingEnabled` keeps its meaning ("are streams routed through the EQ at
  all") and is orthogonal to the mode; a new `SetOutputMode` carries the mode,
  with the same `selected` / `reroute` values as the buttons.
- `capabilities`: keep `output-presets`, add `output-mode` and `monitor-sink`.

## Persistence

`~/.config/mini-eq/output-presets.json` gains, alongside `links` and `default`:

```json
{ "links": {...}, "default": "...", "mode": "selected", "monitor": "follow",
  "version": 2 }
```

Version bump with a read path that tolerates `version: 1` (no mode → Selected).

## Sequencing

1. **Update preset** (overwrite) in the preset panel, enabled when the chip says
   `Modified`. Small, independent, and it retires the "+ and rename" workaround
   immediately. **Done 2026-10-05** (`update_button` in the preset panel), along
   with the fix it exposed: the state chip was only refreshed when a preset was
   loaded or selected, so it kept reading `Saved` while the curve moved away
   from the preset and anything gated on it would never wake up. The chip is now
   recomputed from the live signature in the update loop, only when it changes.
2. **Working follow-default.** Watcher retargets the chain live. Removes the
    "the dropdown does nothing" impression even before the mode is chosen.
    **Done 2026-10-05.** The chain now moves live on a default-sink change
    (`retarget_output`), gated on `output_follows_default` so a pinned device is
    left alone. The poll was pure waste — `refresh_default_audio_sink_name()`
    pumped the PipeWire loop for up to 50 ms per tick to catch a metadata
    property the `property` listener already delivers in real time. It now sets
    a flag (`default_sink_changed`), and the 500 ms timer is a cheap flag read
    via `take_default_sink_change()`.
3. **Mode split.** `RoutingEngine` gains a mode (`Selected` / `Reroute`) and the
    scope predicate; `auto_route_to_sink` takes the scope. Replace the switch with
    the two buttons. **Done 2026-10-05.** `target_in_scope` is pure and tested;
    version 2 of `output-presets.json` persists the mode and the monitor sink;
    `SetOutputMode` is a new D-Bus method and `output_mode` /
    `output_preset` / `monitor_sink` are published in `GetState`.
4. **Curves.** Preset picker + unlink in place of "Link to Output", and debounced
    auto-write-back into a singly-linked preset. **Done 2026-10-05.** The row is
    now `Curve [ <preset> ▾ ] [unlink]`; choosing a preset links it to the active
    device and loads it, unlink drops the link and the fallback applies. The
    model is refilled from the library on a 330 ms cadence. Auto-write-back
    writes the live curve into the linked preset after a 1.5 s debounce on the
    last edit, and only when exactly one device links to it — writing into one
    linked from two would change the curve for a device the user did not touch.
    Built-ins are never written. The comparison is against the preset's own
    saved signature, not the panel's "modified" state.
5. **Monitor device.**
6. Only then reconsider anything about per-app control (see below).

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
- Mode round trip: Reroute moves a stream that Selected leaves alone.
- Follow-default: a default-sink change moves the chain without a rebuild (assert
  the live path, not the log line).
- Update overwrites the selected preset and the chip returns to `Saved`.
- Auto-write-back writes into a singly-linked preset, and does **not** write into
  one linked from two devices (the chip stays `Modified` instead).
- The preset picker links a device to the preset the reader will use on the next
  switch, and unlink removes it.

## Open questions

- Should changing mode while streams are routed re-route immediately, or only on
  the next device change? Re-routing on the spot is more predictable; leaving it
  until the next change is less churn. Default: re-route immediately, since the
  user just told us what they want. **Answered 2026-10-05: yes, re-route
  immediately.** `set_output_mode` re-routes when the engine is already on, so
  the change is visible in the audio path rather than only on the next switch.
- Should "Reroute" remember itself across restarts? Recommend yes (it is in
  the config), with the status line making it obvious. **Answered 2026-10-05:
  yes.** `output_routing_mode` / `set_output_routing_mode` round-trip through
  `output-presets.json` (version 2), and the buttons restore the remembered
  choice on startup.
- Does the monitor device need to survive a chain rebuild? Recommend yes, with
  `Follow EQ output` re-resolving after it. **Answered 2026-10-05: yes, and it
  is persisted.** `output-monitor` in the config is `None` for follow or the
  sink's node name; `resolve_monitor_target` prefers the pinned sink and falls
  back to the chain's current output.
- Auto-write-back debounce: recommend ~1.5 s after the last edit, so a drag does
  not rewrite the file continuously while the chain is reloading anyway.
  **Done 2026-10-05.** The debounce is on the last edit, not the last tick, so a
  drag rewrites once ~1.5 s after it stops rather than continuously.
- Should a preset be protectable from auto-write-back (a "lock" that forces the
  explicit Update path)? Cheap to add and it removes the last reason to be
  nervous about the automatic behaviour. Recommend yes, defaulting to unlocked.
  **Not yet implemented.**
- What happens when a device's linked preset is deleted? Recommend the chip shows
  `Missing`, the link is dropped, and the curve stays as it is rather than
  silently reverting.