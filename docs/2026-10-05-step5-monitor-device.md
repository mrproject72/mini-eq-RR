# 2026-10-05 — Step 5 of the output-modes plan: the monitor device

`docs/PLAN/output-modes-2026-10-05.md` step 5. The monitor always followed the
chain's output, so you could not equalise on headphones and watch the speakers.

## A Monitor device dropdown in the settings popover

`Follow EQ output` (index 0, the default) re-resolves to whatever the chain is
playing to; a specific sink pins the monitor to one device. The model is
refilled from PipeWire on a 330 ms cadence, so a hotplugged device appears
here without a restart. It is listen-only — it does not change where the EQ'd
audio goes, and the tooltip and the status line say so.

The choice is persisted in `output-presets.json` as `monitor`: `None` for
follow, or the sink's node name. On startup the persisted pin decides the
initial selection, so a pinned monitor survives a restart even before the
dropdown has ever been touched. `resolve_monitor_target` prefers the pin and
falls back to the chain's current output, so a pinned monitor also survives a
chain rebuild.

## Verified

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo build --release` all clean.
- `cargo test --lib`: 130 passed.

## Not changed

- Per-app control is still out of scope.
- The output dropdown and the chain still rebuild on a device switch; the
  monitor pin is read at retarget time, so it is unaffected.