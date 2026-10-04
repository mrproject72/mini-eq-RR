#!/usr/bin/env python3
"""Regenerate the hicolor icon set from the master artwork.

Source of truth: `mini_eq_rr_icon_final.png` (1600x1600 RGBA).

Produces, for each size in SIZES:
  assets/icons/<N>x<N>/apps/io.github.mrproject72.mini_eq_rr.png
  assets/icons/<N>x<N>/apps/io.github.mrproject72.mini_eq_rr-symbolic.png

Run from the repository root after replacing the master artwork:

    python3 assets/generate-icons.py

Why this exists rather than being done once by hand: the icons are binaries, so
without a generator there is no way to tell they were derived, or to regenerate
them when the artwork changes. Committing the script keeps the set reproducible.

## Downscaling uses premultiplied alpha

Averaging straight RGBA pixels bleeds the RGB of fully transparent pixels into
the edges. Those pixels carry whatever colour the source stored there -- for this
artwork, near-black -- and the result is a dark halo around the badge. Colour is
therefore multiplied by alpha before averaging and divided out again afterwards:

    a   = alpha / 255
    pm  = rgb * a
    out = mean(pm) / mean(a)

Verified: partial-alpha edge pixels stay magenta (rgb(235, 2, 163) at 16px from
the earlier artwork) instead of darkening.

## Symbolic variants knock the mark out of the badge

A freedesktop *symbolic* icon is a single-colour shape that the toolkit
recolours to match the active theme, so it must not carry the artwork's own
colours.

It is tempting to keep only the alpha channel and discard RGB. That is wrong for
this artwork: the badge is fully opaque, so an alpha silhouette is a featureless
solid block with no mark in it at all -- a black square. The mark is defined by
*colour* (dark on magenta), not by coverage.

Instead the mark is detected by luminance and made transparent, leaving a
single-colour badge with the mark cut out of it, which is the conventional
symbolic treatment for a filled badge. Colour is forced to black, which is the
conventional "currentColor" placeholder -- the toolkit recolours it.

## Note on the source alpha

The master artwork has no partial-alpha pixels: its edges are hard, not
antialiased. Area-averaging still produces correct intermediate coverage at small
sizes (the badge spans many source pixels per output pixel), so the small icons
are fine. At 512px the inherited hard edge is visible. Re-exporting the master
with antialiased edges would improve that, if it is ever worth doing.

No third-party dependencies: PNG decode/encode is done with zlib and numpy, so
this runs anywhere the test suite runs.
"""

import os
import struct
import sys
import zlib

try:
    import numpy as np
except ImportError:
    sys.exit("numpy is required: pip install numpy")

HERE = os.path.dirname(os.path.abspath(__file__))
NAME = "io.github.mrproject72.mini_eq_rr"
SIZES = [16, 24, 32, 48, 64, 128, 256, 512]
SOURCE = os.path.join(HERE, "mini_eq_rr_icon_final.png")


def load_png(path):
    """Minimal RGBA8 PNG decoder (no interlacing)."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path} is not a PNG")
    pos, idat, ihdr = 8, b"", None
    while pos < len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        kind = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + length]
        if kind == b"IHDR":
            ihdr = struct.unpack(">IIBBBBB", chunk)
        elif kind == b"IDAT":
            idat += chunk
        pos += 12 + length
    width, height, depth, color, _, _, interlace = ihdr
    if depth != 8 or color != 6 or interlace != 0:
        raise ValueError("expected 8-bit non-interlaced RGBA")
    channels, stride = 4, width * 4
    raw = zlib.decompress(idat)
    out = np.zeros((height, stride), dtype=np.uint8)
    prev = np.zeros(stride, dtype=np.uint8)
    p = 0
    for y in range(height):
        filt = raw[p]
        p += 1
        line = np.frombuffer(raw[p : p + stride], dtype=np.uint8).astype(np.int32).copy()
        p += stride
        if filt == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif filt == 2:
            line = (line + prev) & 0xFF
        elif filt == 3:
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((a + int(prev[i])) >> 1)) & 0xFF
        elif filt == 4:
            for i in range(stride):
                a = int(line[i - channels]) if i >= channels else 0
                b = int(prev[i])
                c = int(prev[i - channels]) if i >= channels else 0
                q = a + b - c
                pa, pb, pc = abs(q - a), abs(q - b), abs(q - c)
                pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 0xFF
        prev = line.astype(np.uint8)
        out[y] = prev
    return out.reshape(height, width, 4)


def write_png(path, arr):
    height, width, _ = arr.shape
    raw = b"".join(b"\x00" + arr[y].tobytes() for y in range(height))

    def chunk(kind, payload):
        head = struct.pack(">I", len(payload)) + kind + payload
        return head + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF)

    blob = b"\x89PNG\r\n\x1a\n"
    blob += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    blob += chunk(b"IDAT", zlib.compress(raw, 9))
    blob += chunk(b"IEND", b"")
    open(path, "wb").write(blob)


def resize_premultiplied(src, size):
    """Area-average downscale in premultiplied-alpha space. See module docstring."""
    height, width, _ = src.shape
    alpha = src[..., 3].astype(np.float64) / 255.0
    pm = src[..., :3].astype(np.float64) * alpha[..., None]
    out = np.empty((size, size, 4), dtype=np.uint8)
    ys = np.linspace(0, height, size + 1)
    xs = np.linspace(0, width, size + 1)
    for j in range(size):
        y0, y1 = min(int(ys[j]), height), min(max(int(ys[j]) + 1, int(ys[j + 1])), height)
        for i in range(size):
            x0, x1 = min(int(xs[i]), width), min(max(int(xs[i]) + 1, int(xs[i + 1])), width)
            if y1 <= y0 or x1 <= x0:
                out[j, i] = 0
                continue
            coverage = alpha[y0:y1, x0:x1].mean()
            if coverage <= 1e-9:
                out[j, i] = 0
                continue
            mean_pm = pm[y0:y1, x0:x1].reshape(-1, 3).mean(axis=0)
            out[j, i, :3] = np.clip(mean_pm / coverage + 0.5, 0, 255).astype(np.uint8)
            out[j, i, 3] = int(coverage * 255 + 0.5)
    return out


# Rec. 601 luma weights.
_LUMA = np.array([0.299, 0.587, 0.114])


def symbolic(src):
    """Single-colour badge with the mark knocked out, for the active theme.

    An alpha-only silhouette does not work here: the badge is opaque, so it
    would be a featureless black square. The mark is found by luminance instead
    -- the artwork is bright magenta with near-black strokes, so the dark
    fraction is a clean separator -- and made transparent.
    """
    height, width, _ = src.shape
    rgb = src[..., :3].astype(np.float64)
    alpha = src[..., 3].astype(np.float64)

    # The badge is one large flat colour, so its luma is the *mode* of the
    # opaque pixels. Anything meaningfully darker is the mark. Percentiles are
    # unreliable here because after downscaling the mark can be a small
    # minority, and at 16px it barely registers at all.
    opaque = alpha > 200
    luma = rgb @ _LUMA
    if not opaque.any():
        mark = np.zeros((height, width), dtype=bool)
    else:
        badge = np.median(luma[opaque])
        spread = max(1.0, 0.10 * badge)
        mark = (luma < badge - spread) & opaque

    out = np.zeros((height, width, 4), dtype=np.uint8)
    # Smooth the knockout so the mark does not alias into ragged pixels.
    frac = mark.astype(np.float64)
    if height > 2 and width > 2:
        frac = (
            frac
            + np.roll(frac, 1, axis=0)
            + np.roll(frac, -1, axis=0)
            + np.roll(frac, 1, axis=1)
            + np.roll(frac, -1, axis=1)
        ) / 5.0
        frac = np.clip((frac - 0.2) / 0.6, 0.0, 1.0)
    out[..., 3] = np.clip(alpha * (1.0 - frac), 0, 255).astype(np.uint8)
    return out


def check_not_banded(path):
    """Fail if an icon came out as horizontal bands.

    Regression guard. An earlier version of `resize_premultiplied` indexed the
    column edges with the *row* variable, so every output row sampled one single
    x-window. The result was flat horizontal stripes of varying thickness with
    the artwork's mark averaged away -- which is exactly what it looked like.

    The signature is that each row is internally uniform, so no row ever
    contains both the badge colour and the darker mark. With the artwork's mark
    present, plenty of rows must.
    """
    img = load_png(path)
    size = img.shape[0]
    if size < 32:
        return  # Too small for the mark to survive; nothing to assert.
    alpha = img[..., 3]
    luma = img[..., :3].astype(np.float64) @ _LUMA
    opaque = alpha > 200
    if not opaque.any():
        raise SystemExit(f"{path}: no opaque pixels")
    badge = np.median(luma[opaque])
    dark = opaque & (luma < badge - max(1.0, 0.10 * badge))
    mixed = sum(1 for y in range(size) if dark[y].any() and (opaque[y] & ~dark[y]).any())
    if mixed < 3:
        raise SystemExit(
            f"{path}: only {mixed} row(s) contain both badge and mark -- the icon is "
            "probably banded; see the resize_premultiplied index handling"
        )


def main():
    for size in SIZES:
        target = os.path.join(HERE, "icons", f"{size}x{size}", "apps")
        os.makedirs(target, exist_ok=True)
        small = resize_premultiplied(src, size)
        main_path = os.path.join(target, f"{NAME}.png")
        write_png(main_path, small)
        check_not_banded(main_path)
        # Scale FIRST, then knock the mark out at that resolution. Doing it the
        # other way round does not work: the mark is only a couple of source
        # pixels wide, so knocking it out at 1600px and then averaging produces
        # ~20% alpha at 64px, which reads as solid badge again.
        write_png(os.path.join(target, f"{NAME}-symbolic.png"), symbolic(small))
        print(f"wrote {size}x{size}")


def check():
    """Re-derive every icon and confirm the committed files match, byte for byte."""
    src = load_png(SOURCE)
    bad = []
    for size in SIZES:
        target = os.path.join(HERE, "icons", f"{size}x{size}", "apps")
        small = resize_premultiplied(src, size)
        for name, want in (
            (f"{NAME}.png", small),
            (f"{NAME}-symbolic.png", symbolic(small)),
        ):
            path = os.path.join(target, name)
            if not os.path.exists(path):
                bad.append(f"missing {path}")
                continue
            if load_png(path).tobytes() != want.tobytes():
                bad.append(f"{path} differs from what the generator produces")
        check_not_banded(os.path.join(target, f"{NAME}.png"))
    if bad:
        raise SystemExit("icon check failed:\n  " + "\n  ".join(bad))
    print(f"icons OK: {len(SIZES)} sizes match the generator and none are banded")


if __name__ == "__main__":
    if "--check" in sys.argv:
        check()
    else:
        main()