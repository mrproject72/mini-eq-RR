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

Two details the generator handles that are easy to get wrong, both explained in
`generate-icons.py`:

- **Premultiplied-alpha downscaling.** Averaging straight RGBA bleeds the RGB of
  fully transparent pixels into the edges, which shows as a dark halo.
- **Symbolic variants are alpha silhouettes**, because the toolkit recolours them
  to the active theme and they must not carry the artwork's own colours.

`install-desktop` copies these into `$XDG_DATA_HOME/icons/hicolor/…`, and
`desktop_integration::tests` asserts every size is present and is a real PNG.

## AppStream metadata

`metainfo/io.github.mrproject72.mini_eq_rr.metainfo.xml` — required by the
deb, rpm and Flatpak packages for software centres to show the application.
`<id>` must match `core::APP_ID`.