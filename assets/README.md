# Application assets

## Icons

Master artwork: `mini_eq_rr_icon_final.png` (1600×1600 RGBA).

`icons/` is **generated** — do not edit those files by hand. After replacing the
master artwork, regenerate everything with:

```bash
python3 assets/generate-icons.py
```

That writes, for each of 16, 24, 32, 48, 64, 128, 256 and 512 px:

```
icons/<N>x<N>/apps/io.github.mrproject72.mini_eq_rr.png
icons/<N>x<N>/apps/io.github.mrproject72.mini_eq_rr-symbolic.png
```

`parametric_eq_mixer_icon.png` is the earlier draft of the same design (softer
magenta, thinner glyph, antialiased edges). It is kept for reference; it is not
the source of truth.

Four details the generator handles that are easy to get wrong, all explained in
`generate-icons.py`:

- **A Lanczos filter stretched to the reduction factor.** Area averaging is a box
  filter: right for coverage, but it aliases. At 1600→16 (100×) the small icons
  came out blurred and speckled. Stretching a windowed sinc over the whole
  source footprint of each destination pixel is what actually anti-aliases. The
  result measures ~1.6× the edge energy of `convert -filter Lanczos` at the same
  sizes.
- **Premultiplied-alpha downscaling.** Averaging straight RGBA bleeds the RGB of
  fully transparent pixels into the edges, which shows as a dark halo.
- **Symbolic variants knock the mark out, they are not alpha silhouettes.** An
  alpha-only silhouette is wrong here: the badge is opaque, so it would be a
  featureless black square with no mark in it. The mark is found by luminance and
  made transparent, leaving a single-colour badge with the mark cut out.
- **Scale first, knock out afterwards.** Reversing this silently undoes the work:
  the mark is only a couple of source pixels wide, so knocking it out at 1600px
  and then averaging leaves ~20% alpha at 64px, which reads as solid badge again.

### Verifying

```bash
python3 assets/generate-icons.py --check
```

This re-derives every icon, compares byte-for-byte with what is committed, and
fails on either of two defects that both shipped unnoticed once already:

- **Banding.** An earlier resampler indexed the column edges with the row
  variable, so every output row sampled one x-window and the artwork came out as
  flat horizontal stripes of differing thickness. `check_not_banded()` asserts
  that several rows contain both the badge colour and the darker mark, which
  banding cannot produce.
- **Blur.** `check_not_blurred()` compares the icon's edge energy against a plain
  box average of the same source and fails unless the icon is meaningfully
  sharper. It is self-validating — no tuned threshold — and fails automatically
  if the Lanczos resampler is swapped back for averaging.

Both guards were verified by reintroducing their respective bug. The check runs
in CI.

### Known limitation

At 16px and 24px the mark is only just legible: the artwork's strokes are thin
relative to those sizes. Small icons usually want bolder, simplified artwork
rather than a scaled-down full-size design. That is a design decision, not a
resampling one, so it has not been done here.

`install-desktop` copies these into `$XDG_DATA_HOME/icons/hicolor/…`, and
`desktop_integration::tests` asserts every size is present and is a real PNG.

## AppStream metadata

`metainfo/io.github.mrproject72.mini_eq_rr.metainfo.xml` — required by the
deb, rpm and Flatpak packages for software centres to show the application.
`<id>` must match `core::APP_ID`.