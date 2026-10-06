# 2026-10-05 — Step 2 of the output-modes plan: follow-default that actually follows

`docs/PLAN/output-modes-2026-10-05.md` step 2. The dropdown's index 0 ("Default
Output (follow system)") was a label with no behaviour: the chain's output was
fixed when the module loaded, so going back to the default had to rebuild the
module — and the 500 ms watcher that was supposed to notice the default
changing only moved the **monitor**, with a comment saying the chain re-link
was "deliberately NOT attempted". Both halves of the same lie.

## The chain now follows the default

`retarget_output` already had a cheap live-move path (`move_stream_to_target_checked`,
verified by reading the target back), so the watcher's refusal was stale, not
cautious. The watcher now calls `be.retarget_output(&new_default)` when the
default changes and the user has not pinned a device:

- live move takes → nothing rebuilt, no stream touched, inaudible;
- live move fails → falls back to the rebuild path, which hands streams back
  before the module goes away and re-routes only if they were routed.

After a successful retarget the watcher also applies the new device's preset
(`apply_output_preset_for_sink`) and updates `AppState::output_sink` /
`StateChanged`, exactly as the dropdown does. The monitor is retargeted after
the chain, only when it was already following the chain — a user's own monitor
device is left alone.

Gated on `output_follows_default` (the `Cell` the dropdown sets), so picking a
specific device pins the chain and this watcher cannot pull it away.

## The poll was pure waste; it is now a flag read

`refresh_default_audio_sink_name()` pumped the PipeWire loop for up to 50 ms on
every 500 ms tick, to catch a metadata property that the `property` listener on
the `default` metadata object already delivers in real time. The listener was
already bound (it populates `default_audio_sink` and the `target_cache`); it was
just never wired to a signal.

- `RoutingEngine` gains `default_sink_changed: Rc<RefCell<bool>>`, set by the
  listener when `default.audio.sink` arrives.
- `current_default_audio_sink()` reads the cache without pumping.
- `take_default_sink_change()` returns the new sink and clears the flag, or
  `None` — so the 500 ms timer is a cheap flag read on the common case and acts
  on a change exactly once.
- `default_audio_sink_name()` keeps its initial-bind pump (the cache is empty
  until the server replays) and drops the per-tick pump from `refresh_*`,
  which is deleted; `pipewire_backend.rs` exposes `take_default_sink_change()`.
- `refresh_default_audio_sink_name` is gone from the public surface; nothing
  else called it.

The 500 ms cadence is now only the debounce between acting on the flag.

## Verified

- `cargo build --release`, `cargo test --lib` (123 passed), `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings` all clean.
- Live: the chain retargets on a real default-sink change via the live path
  (logged `EQ output moved live to … (no rebuild)`). Whether the switch is
  gapless needs an ear — the live move does not touch any playback stream, but
  the destination node's ports still relink.

## Not changed

- The dropdown still rebuilds the module when a specific device is chosen; the
  live move is attempted first and only falls back.
- `output_follows_default` is still `true` at startup, matching the old index-0
  behaviour — except index 0 now does something.