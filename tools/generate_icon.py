"""Generate the Jinfu Search icon set: high-res PNG, multi-size ICO, and
embedded RGBA byte arrays for the tray and window icons (no image crate needed).
Run: python tools/generate_icon.py  (from repo root)
"""
from __future__ import annotations

import math
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
SRC = ROOT / "src"

# Palette: deep blue base + gold magnifier (jin=gold, fu=fortune)
BLUE_EDGE = (10, 42, 102, 255)      # deep navy
BLUE_CORE = (59, 130, 246, 255)     # bright blue
BLUE_HILITE = (147, 197, 253, 255)  # light blue for glow
GOLD_DARK = (166, 124, 12, 255)
GOLD_MID = (240, 180, 41, 255)
GOLD_LIGHT = (253, 224, 138, 255)
WHITE = (255, 255, 255, 255)

SIZE = 512
SS = 4  # supersampling for smooth anti-aliased edges


def radial_mask(size: int) -> Image.Image:
    """White disc on transparent background."""
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.ellipse([0, 0, size - 1, size - 1], fill=(255, 255, 255, 255))
    return img


def lerp(a: tuple, b: tuple, t: float) -> tuple:
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(len(a)))


def radial_gradient_fill(size: int) -> Image.Image:
    """Blue radial gradient: bright core -> deep navy edge, cropped to a disc."""
    grad = Image.radial_gradient("L").resize((size, size), Image.BILINEAR)
    # radial_gradient: center is 0 (black), edge is 255 (white) in PIL.
    # We want core = BLUE_CORE, edge = BLUE_EDGE, so use (255 - v).
    lut = [lerp(BLUE_CORE, BLUE_EDGE, (255 - v) / 255.0) for v in range(256)]
    flat = bytes(c for px in lut for c in px)
    raw = grad.tobytes()
    colored = Image.frombytes("RGBA", (size, size), bytes(c for g in raw for c in flat[g * 4:g * 4 + 4]))
    return colored


def draw_icon(px: int) -> Image.Image:
    """Render the icon at final pixel size (supersampled internally)."""
    w = px * SS
    img = Image.new("RGBA", (w, w), (0, 0, 0, 0))

    margin = w * 0.045
    disc = w - 2 * margin
    top, left = margin, margin

    # --- 1. base disc: radial blue gradient + subtle top sheen --------------
    base = radial_gradient_fill(w)
    mask = radial_mask(w)
    base.putalpha(mask.getchannel("A"))
    # top sheen (soft light from upper-left)
    sheen = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    sd = ImageDraw.Draw(sheen)
    sd.ellipse(
        [left + disc * 0.14, top + disc * 0.05, left + disc * 0.86, top + disc * 0.45],
        fill=(255, 255, 255, 60),
    )
    sheen = sheen.filter(ImageFilter.GaussianBlur(w * 0.035))
    base = Image.alpha_composite(base, sheen)

    # disc frame: subtle darker rim
    rim = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    rd = ImageDraw.Draw(rim)
    rd.ellipse([left, top, left + disc, top + disc], outline=(0, 0, 0, 90), width=max(2, w // 170))
    base = Image.alpha_composite(base, rim)

    # --- 2. orbit arc (whole-disk scan) at bottom of the disc ---------------
    orbit = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    od = ImageDraw.Draw(orbit)
    cx = w / 2
    cy = top + disc * 0.78
    rx = disc * 0.46
    ry = disc * 0.16
    od.arc([cx - rx, cy - ry, cx + rx, cy + ry], start=200, end=340, fill=(255, 255, 255, 70), width=max(2, w // 180))
    # three "file dots" sitting on the orbit
    for t, color in ((0.30, BLUE_HILITE), (0.47, GOLD_MID), (0.64, WHITE)):
        a = math.radians(200 + 140 * t)
        px_ = cx + rx * math.cos(a)
        py_ = cy + ry * math.sin(a)
        r = w * 0.030
        od.ellipse([px_ - r, py_ - r, px_ + r, py_ + r], fill=color[:3] + (230,))
    orbit = orbit.filter(ImageFilter.GaussianBlur(w * 0.004))
    base = Image.alpha_composite(base, orbit)

    # --- 3. magnifier: gold ring + lens + handle ----------------------------
    lens_c = (w * 0.54, w * 0.40)
    ring_r = w * 0.235
    ring_w = max(3, w * 0.052)

    # gold ring with vertical light->dark gradient
    ring = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    rd = ImageDraw.Draw(ring)
    rd.ellipse(
        [lens_c[0] - ring_r, lens_c[1] - ring_r, lens_c[0] + ring_r, lens_c[1] + ring_r],
        fill=GOLD_MID,
    )
    # inner punch-out: replace inner disc with transparent using a mask
    inner = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    idr = ImageDraw.Draw(inner)
    ir = ring_r - ring_w
    idr.ellipse(
        [lens_c[0] - ir, lens_c[1] - ir, lens_c[0] + ir, lens_c[1] + ir],
        fill=(255, 255, 255, 255),
    )
    ring.putalpha(ImageChops.subtract(ring.getchannel("A"), inner.getchannel("A")))

    # vertical gold gradient on the ring via paste + mask
    grad_lut = [lerp(GOLD_LIGHT, GOLD_DARK, v / (w - 1)) for v in range(w)]
    grad_flat = b"".join(bytes(px) * w for px in grad_lut)  # one row color per y
    grad_img = Image.frombytes("RGBA", (w, w), grad_flat)
    ring = Image.composite(grad_img, ring, ring.getchannel("A"))

    # dark rim on outer & inner edge for depth
    rim2 = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    rd2 = ImageDraw.Draw(rim2)
    rd2.ellipse(
        [lens_c[0] - ring_r, lens_c[1] - ring_r, lens_c[0] + ring_r, lens_c[1] + ring_r],
        outline=(90, 60, 0, 140), width=max(1, w // 340),
    )
    ring = Image.alpha_composite(ring, rim2)

    # lens glass: translucent white + specular highlight
    lens = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    ld = ImageDraw.Draw(lens)
    lr = ring_r - ring_w
    ld.ellipse(
        [lens_c[0] - lr, lens_c[1] - lr, lens_c[0] + lr, lens_c[1] + lr],
        fill=(255, 255, 255, 46),
    )
    # specular crescent at top-left of the glass
    ld.arc(
        [lens_c[0] - lr * 0.92, lens_c[1] - lr * 0.92, lens_c[0] + lr * 0.92, lens_c[1] + lr * 0.92],
        start=215, end=330, fill=(255, 255, 255, 150), width=max(2, w // 130),
    )
    lens = lens.filter(ImageFilter.GaussianBlur(w * 0.006))
    img = Image.alpha_composite(img, base)
    img = Image.alpha_composite(img, lens)

    # --- 4. folder inside the lens (the found file) --------------------------
    folder = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    fd = ImageDraw.Draw(folder)
    fw = ring_r * 1.16   # folder width
    fh = ring_r * 0.86   # folder height
    fx0 = lens_c[0] - fw / 2
    fy0 = lens_c[1] - fh / 2 + ring_r * 0.06
    tab_h = fh * 0.30
    # back tab
    fd.rounded_rectangle(
        [fx0 + fw * 0.06, fy0, fx0 + fw * 0.5, fy0 + tab_h], radius=fh * 0.08, fill=WHITE,
    )
    # main body
    fd.rounded_rectangle(
        [fx0, fy0 + tab_h * 0.7, fx0 + fw, fy0 + fh], radius=fh * 0.09, fill=WHITE,
    )
    # inner lines to suggest paper
    line_col = (80, 120, 200, 255)
    for i in range(3):
        ly = fy0 + tab_h * 0.7 + fh * 0.26 + i * fh * 0.14
        lx0 = fx0 + fw * 0.16
        lx1 = fx0 + fw * (0.72 if i < 2 else 0.52)
        fd.line([lx0, ly, lx1, ly], fill=line_col, width=max(1, w // 220))
    folder = folder.filter(ImageFilter.GaussianBlur(w * 0.003))
    img = Image.alpha_composite(img, folder)

    # --- 5. handle (rotated rounded bar) -------------------------------------
    handle = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    hd = ImageDraw.Draw(handle)
    hw = ring_w * 0.92
    hl = ring_r * 1.28
    hx0 = lens_c[0] + ring_r * 0.72 - hw / 2
    hy0 = lens_c[1] + ring_r * 0.72 - hl / 2
    hd.rounded_rectangle([hx0, hy0, hx0 + hw, hy0 + hl], radius=hw / 2, fill=GOLD_MID)
    # gradient on handle (light top-left -> dark bottom-right)
    handle = handle.rotate(-45, center=(w / 2, w / 2), resample=Image.BICUBIC, expand=False)
    hmask = handle.getchannel("A")
    handle = Image.composite(grad_img, handle, hmask)
    # rim
    rim3 = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    rd3 = ImageDraw.Draw(rim3)
    rd3.rounded_rectangle([hx0, hy0, hx0 + hw, hy0 + hl], radius=hw / 2, outline=(90, 60, 0, 130), width=max(1, w // 400))
    rim3 = rim3.rotate(-45, center=(w / 2, w / 2), resample=Image.BICUBIC, expand=False)
    handle = Image.alpha_composite(handle, rim3)
    img = Image.alpha_composite(img, handle)

    # --- 6. soft drop shadow under the whole disc ----------------------------
    shadow = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    sd = ImageDraw.Draw(shadow)
    sd.ellipse([left + disc * 0.03, top + disc * 0.07, left + disc * 0.97, top + disc * 1.02], fill=(0, 0, 0, 70))
    shadow = shadow.filter(ImageFilter.GaussianBlur(w * 0.025))
    # place shadow behind: composite shadow under current image
    out = Image.alpha_composite(shadow, img)

    return out.resize((px, px), Image.LANCZOS)


def to_rgba_table(img: Image.Image, px: int) -> str:
    """Render a compact Rust byte-array literal for `px` icon."""
    img = img.resize((px, px), Image.LANCZOS)
    data = list(img.tobytes())
    rows = []
    for i in range(0, len(data), 24):
        rows.append("    " + ", ".join(str(v) for v in data[i:i + 24]) + ",")
    return "\n".join(rows)


def main() -> None:
    ASSETS.mkdir(exist_ok=True)
    icon = draw_icon(SIZE)

    icon.save(ASSETS / "icon-512.png")
    # multi-size ico (Windows icon set)
    icon.save(
        ASSETS / "icon.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    # 32x32 tray + 16x16 small variants
    tray32 = icon.resize((32, 32), Image.LANCZOS)
    tray32.save(ASSETS / "icon-32.png")
    tray16 = icon.resize((16, 16), Image.LANCZOS)
    tray16.save(ASSETS / "icon-16.png")

    # embedded RGBA tables for tray icon (no image crate at runtime)
    header = (
        "// @generated by tools/generate_icon.py — do not edit by hand.\n"
        "// 32x32 tray icon RGBA bytes (row-major).\n"
        f"pub const TRAY_ICON_32: &[u8] = &[\n{to_rgba_table(tray32, 32)}\n];\n"
    )
    (SRC / "icon_data.rs").write_text(header, encoding="utf-8")
    print(f"wrote {ASSETS / 'icon-512.png'}, {ASSETS / 'icon.ico'}, {SRC / 'icon_data.rs'}")
    print(f"tray32 corner alpha: {tray32.getpixel((0, 0))}")


if __name__ == "__main__":
    main()
