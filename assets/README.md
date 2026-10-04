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

Three details the generator handles that are easy to get wrong, all explained in
`generate-icons.py`:

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

This re-derives every icon and compares byte-for-byte with what is committed, and
fails if any icon is *banded*. The band check exists because of a real bug: an
earlier version of the resampler indexed the column edges with the row variable,
so every output row sampled one x-window and the artwork came out as flat
horizontal stripes of differing thickness. `check_not_banded()` asserts that
several rows contain both the badge colour and the darker mark, which banding
cannot produce. It runs in CI.

`install-desktop` copies these into `$XDG_DATA_HOME/icons/hicolor/…`, and
`desktop_integration::tests` asserts every size is present and is a real PNG.

## AppStream metadata

`metainfo/io.github.mrproject72.mini_eq_rr.metainfo.xml` — required by the
deb, rpm and Flatpak packages for software centres to show the application.
`<id>` must match `core::APP_ID`.