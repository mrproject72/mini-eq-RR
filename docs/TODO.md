## Session 2026-10-10 backlog (prioritised)

The flatpak EQ now works end-to-end (native-biquad graph, sandbox push,
drag editing, exit reliability). Ranked next steps:

1. **Graph parity polish (steps 6-9 of
   `docs/PLAN/graph-parity-2026-10-04.md`)** — IN PROGRESS 2026-10-10:
   - A/B-bypass curve styling (grey/half-alpha when bypassed), gradient
     area fill under the curve, low-alpha glow, the selected band's own
     contribution curve
   - Focus label under the graph (`Band 3 • 1.0k • +6.0 dB`), the
     `N Bands` band-count chip, solo-effectiveness fader dimming
   - Fader drag/scroll/arrow-keys select the band
   - LUFS meter draw func in the monitor strip
2. **Preset lifecycle completion** — save/save-as, delete, import/export
   (rename/revert/monitoring already exist).
3. **Controller-pattern refactor** — one authoritative band list
   (upstream `controller.bands`) instead of the fader widgets as source +
   the tick's snapshot; enables upstream's 16 ms per-band DSP pushes
   during drags (ours debounces the full payload at 400 ms).
4. **Unit-test coverage for the backend** — `pipewire_backend.rs` and
   `routing.rs` have near-zero automated coverage; the routing edge cases
   hit this week (stale targets, prefix-push semantics, scope rules) are
   only protected by the gold path.
5. **Flathub submission** under our own app ID; then the GNOME Shell
   extension from upstream.
6. **AGENTS.md phase status refresh** — stale: it still says the graph
   has no drag editing and Phase 8's flatpak is pending; both done.

# TODO

Low-priority backlog. Nothing here blocks a release.

## CI / multi-distro pipeline

CI is currently green on `ubuntu-24.04` after dropping the libadwaita
`v1_7` requirement (WrapBox -> GTK4 FlowBox), so the app builds against
libadwaita 1.5 as shipped by Ubuntu 24.04 LTS.

Ranked by value, all via `container:` jobs (GitHub-hosted runners are
Ubuntu-only):

1. **Containerised PipeWire smoke test — highest value.** All 90 tests are
   unit tests; `pipewire_backend.rs` and `routing.rs` have zero automated
   coverage and are only verified on a dev machine. PipeWire can run in a
   container against a dummy/null sink, which would let CI:
   - create the filter-chain and assert `mini_eq_sink` appears
   - exercise route/unroute and assert streams are handed back to the
     default output (regression test for the "audio stops when the app
     closes" bug)
   - confirm biquad coefficients actually reach the node
2. ~~**Flatpak build job**~~ — DONE 2026-10-06: `io.github.mrproject72.mini_eq_rr.yml`
   + `flatpak` job in `ci.yml`; appstream/desktop validated.
3. **Fedora** (`fedora:latest`) — largest PipeWire + GNOME desktop overlap;
   exercises the RPM dependency path.
4. **Arch** (`archlinux:latest`) — rolling, so it surfaces libadwaita /
   GTK API drift early, before it reaches users.
5. **Debian stable** (`debian:trixie`) — worthwhile but conservative
   packages make the signal slow-moving.
6. **CachyOS** (`ghcr.io/cachyos/docker`) — Arch-based, so build coverage
   duplicates Arch. Its real differentiator is x86-64-v3/v4 optimised
   packages and a tuned kernel, so the useful job there is a **CPU
   benchmark** (measured %CPU under load vs a stock target) rather than a
   compile check — actual evidence for the project's low-CPU premise.

Skip: Alpine/musl — not a realistic PipeWire desktop target.

## CI / release flow

The current `.github/workflows/ci.yml` runs `fmt`, `clippy -D warnings`,
`check --release`, `test --lib` and `build --release` on push and PR to
`main`. Gaps worth closing eventually:

- **Branch protection on `main`** — require the CI check to pass and at
  least one review before merge. Right now nothing enforces it.
- **Release artifacts** — CI now produces the flatpak repo via `flatpak-builder`;
  still add `actions/upload-artifact` for the release binary and a `.deb`.
- **Tag-triggered releases** — no workflow on `push: tags: [v*]`, so
  tagging does nothing.
- **No integration test on live PipeWire** — all 90 tests are unit tests.
  The backend (`pipewire_backend.rs`, `routing.rs`) is only verified on a
  developer machine. A containerised PipeWire smoke test would catch real
  regressions.
- **No coverage reporting.**

## Version numbering and changelog

The crate has sat at `0.9.0` for the whole rewrite with no changelog
discipline. Needs:

- a real version scheme from a numbered first release
- `CHANGELOG.md` (Keep a Changelog format), back-filled from
  `docs/*-updates.md`
- README updated with the version history

## Filter-chain output re-target (needs live validation)

The monitor now follows a system default-output change, but the
filter-chain's own output does not. Implementing it means removing the
existing `mini_eq_sink_output -> old_sink` link and creating a new one.

Do NOT ship this without testing on real hardware: a wrong link produces
either silence or a feedback loop. Needs a machine with at least two
output devices and a live sink switch to validate against.

Building blocks already present in `routing.rs`: `find_node_id_by_name`,
`find_node_target`, `get_links` (RouteInfo has source/target/link ids),
`remove_link`, `link_nodes`.

## Feature gaps

- **Preset lifecycle** — no save/save-as, import/export/delete, fallback
  presets. (Revert/reapply, file monitoring, and output-preset linking
  are implemented.)
- **Fader bottom residual clip** at some window sizes (see Known Issues).
- **Flatpak packaging** and **GNOME Shell extension**.
- **Config migration** — the app ID and config directory were renamed to
  `io.github.mrproject72.mini_eq_rr` / `~/.config/mini-eq-rr`. Existing
  settings under the old `~/.config/mini-eq` are **not** picked up. If
  that matters, add a one-shot migration on first run.
