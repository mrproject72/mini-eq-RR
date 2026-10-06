# 2026-10-05 — Step 4 of the output-modes plan: the curve picker and auto-write-back

`docs/PLAN/output-modes-2026-10-05.md` step 4. The "Link to Output" button wrote the
sink's node name into a value label that nothing ever read back, so a device could
be "linked" to a preset and the link would never be applied.

## The link is now a picker

The row is `Curve [ <preset> ▾ ] [unlink]`. Choosing a preset in the dropdown links
it to the active device immediately, so the reader will use it on the next switch —
and it loads that preset onto the curve now, so what you see is what the device gets.
The unlink button next to it drops the link and the fallback applies instead.

The model is refilled from the preset library on a 330 ms cadence, so a preset
created or deleted elsewhere shows up here without a restart, and the selection
tracks the active device's linked preset. The "(none)" entry means the fallback
applies.

**Set Fallback** stays as it is: one global preset for devices with no link.

## Auto-write-back

Editing the EQ while a device is linked should not require remembering to press
Update. When the live curve moves away from the preset linked to the active device,
it is written into that preset after a 1.5 s debounce — on the last edit, not the
last tick, so a drag rewrites once after it settles rather than continuously.

It only writes into a preset linked from **exactly one** device. Writing into one
linked from two would silently change the curve for a device the user did not
touch, which is worse than leaving it Modified. Built-ins are never written.

The comparison is against the preset's own saved signature
(`output_preset_saved_signature_for_sink`), not against the panel's "modified"
state, so the write-back is independent of which preset is selected in the panel.

## Verified

- `cargo test --lib`: 130 passed, including
  `auto_write_output_preset_for_sink_writes_only_for_singly_linked_preset`
  (writes for one link, refuses for two, does nothing for an unlinked sink) and
  `saved_signature_for_linked_sink_matches_the_preset`.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo build --release` all clean.

## Not changed

- The monitor-device dropdown is step 5.
- Per-app control is still out of scope.
- The debounce is 1.5 s; the plan's open question about a lock is still open.