#!/usr/bin/env python3
"""Draws the rich-menu images (SRS §3.1.2, FR-UI-11) from menus.json, next to this script, and
the photo placeholder of product cards without a photo (FR-SRCH-07), ../placeholder.png.

    python3 assets/rich-menus/render.py

menus.json is the menus' layout as the application defines it, in
src/notifications/rich_menus.rs: each menu's bar of tabs and the cells of its functions, with
their text and where they are tapped. A test keeps it in step with the code. When a label or tap
area changes there, the test prints the new menus.json: save it here, run this script, and
commit the images with it.

Needs Pillow with raqm, which shapes the Thai text:
    python3 -c "from PIL import features; print(features.check('raqm'))"
The fonts are downloaded at pinned versions into .fonts/ on the first run, and checked:
- Sarabun Bold (SIL Open Font License 1.1), whose looped letters are easier for older readers
- Material Symbols Rounded (Apache License 2.0), for the icons
"""

import hashlib
import json
import sys
import urllib.request
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont, features

HERE = Path(__file__).resolve().parent
FONTS = HERE / ".fonts"

GOOGLE_FONTS = "https://raw.githubusercontent.com/google/fonts/7d82a06388d70ec34312df7e7cede76ba8bbf7b5"
MATERIAL = (
    "https://raw.githubusercontent.com/google/material-design-icons/"
    "10ecc14790612ca8fd164e3f4ffe9c16fc50d5b7/variablefont"
)
SYMBOLS = "MaterialSymbolsRounded%5BFILL%2CGRAD%2Copsz%2Cwght%5D"
# File, where it comes from, and its SHA-256
DOWNLOADS = [
    (
        "Sarabun-Bold.ttf",
        f"{GOOGLE_FONTS}/ofl/sarabun/Sarabun-Bold.ttf",
        "d38308ca27d067b9a1b79006337c1f9a66c29aa016d96a9d88d3801da7fa83b9",
    ),
    (
        "MaterialSymbolsRounded.ttf",
        f"{MATERIAL}/{SYMBOLS}.ttf",
        "7e74e36533ccee12c2b81e75bfcf3ccf123d3bb7e7e46d65d94445f23db0c97c",
    ),
    (
        "MaterialSymbolsRounded.codepoints",
        f"{MATERIAL}/{SYMBOLS}.codepoints",
        "a7404d9e5a1b63f496cf05ace45d386075886b5a7f123fba9f29527cb6006fde",
    ),
]

# Drawn this many times larger, then scaled down, so edges are smooth
SCALE = 2

# Each tab's colours: strong for its tab and icons, light behind its functions
COLOURS = {
    "buyer": {"strong": (14, 128, 64), "light": (229, 244, 235)},
    "seller": {"strong": (196, 72, 10), "light": (253, 238, 226)},
    "admin": {"strong": (28, 86, 196), "light": (231, 239, 252)},
}
TEXT = (31, 41, 51)
CARD = (255, 255, 255)
CARD_EDGE = (214, 220, 226)
IDLE_TAB = (236, 239, 242)
IDLE_TEXT = (84, 96, 108)
# The product photo placeholder: its size, and the colour of its icon and text
PLACEHOLDER = {"width": 1040, "height": 676}
PLACEHOLDER_INK = (150, 160, 170)

TAB_ICONS = {"buyer": "shopping_basket", "seller": "storefront", "admin": "shield_person"}
ICONS = {
    "buy": "shopping_basket",
    "search": "search",
    "cart": "shopping_cart",
    "my_orders": "receipt_long",
    "my_shop": "storefront",
    "help": "help",
    "shop_orders": "assignment",
    "add_product": "add_box",
    "my_products": "inventory_2",
    "open_close": "toggle_on",
    "sales": "bar_chart",
    "shop_settings": "settings",
    "approve_shops": "verified",
    "review_queue": "fact_check",
    "users": "group",
    "categories": "category",
    "stats": "query_stats",
    "announcements": "campaign",
}


def fetch_fonts():
    FONTS.mkdir(exist_ok=True)
    for name, url, sha256 in DOWNLOADS:
        path = FONTS / name
        if not path.exists():
            print(f"downloading {name}")
            with urllib.request.urlopen(url) as response:
                path.write_bytes(response.read())
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        if digest != sha256:
            path.unlink()
            sys.exit(f"{name} is not the pinned version (SHA-256 {digest}); run again")


class Fonts:
    def __init__(self):
        self.text_path = str(FONTS / "Sarabun-Bold.ttf")
        self.icon_path = str(FONTS / "MaterialSymbolsRounded.ttf")
        self.codepoints = {}
        for line in (FONTS / "MaterialSymbolsRounded.codepoints").read_text().splitlines():
            name, codepoint = line.split()
            self.codepoints[name] = chr(int(codepoint, 16))

    def text(self, size):
        return ImageFont.truetype(self.text_path, size, layout_engine=ImageFont.Layout.RAQM)

    def icon(self, size):
        font = ImageFont.truetype(self.icon_path, size, layout_engine=ImageFont.Layout.BASIC)
        # Filled, at the largest optical size, medium weight
        font.set_variation_by_axes([1, 0, 48, 500])
        return font

    def glyph(self, name):
        return self.codepoints[name]


def draw_icon(image, fonts, name, centre, size, colour):
    """Icon `name`, `size` high, centred on `centre`. The variable font's contours overlap, and
    FreeType leaves thin gaps where they do, so the icon is drawn larger, its gaps closed, and
    scaled down."""
    big = size * 2
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).text(
        (big / 2, big / 2), fonts.glyph(name), font=fonts.icon(big), fill=255, anchor="mm"
    )
    mask = mask.filter(ImageFilter.MaxFilter(11)).filter(ImageFilter.MinFilter(11))
    mask = mask.resize((size, size), Image.Resampling.LANCZOS)
    left, top = round(centre[0] - size / 2), round(centre[1] - size / 2)
    image.paste(Image.new("RGB", (size, size), colour), (left, top), mask)


def scaled(bounds):
    return {key: value * SCALE for key, value in bounds.items()}


def fitted(fonts, lines, size, width):
    """The text font at `size`, or smaller until each of `lines` is at most `width` wide."""
    while True:
        font = fonts.text(size)
        if max(font.getlength(line) for line in lines) <= width:
            return font
        size -= 2


def draw_tab(image, draw, fonts, tab):
    b = scaled(tab["bounds"])
    colours = COLOURS[tab["tab"]]
    fill, ink = (colours["strong"], CARD) if tab["active"] else (IDLE_TAB, IDLE_TEXT)
    gap = 6 * SCALE
    draw.rectangle(
        [b["x"] + gap, b["y"] + gap, b["x"] + b["width"] - gap - 1, b["y"] + b["height"] - 1],
        fill=fill,
    )
    font = fonts.text(104 * SCALE)
    icon_size = 120 * SCALE
    space = 28 * SCALE
    width = icon_size + space + font.getlength(tab["label"])
    left = b["x"] + (b["width"] - width) / 2
    middle = b["y"] + gap + (b["height"] - gap) / 2
    draw_icon(image, fonts, TAB_ICONS[tab["tab"]], (left + icon_size / 2, middle), icon_size, ink)
    draw.text((left + icon_size + space, middle), tab["label"], font=font, fill=ink, anchor="lm")


def draw_cell(image, draw, fonts, cell, tab):
    b = scaled(cell["bounds"])
    colours = COLOURS[tab]
    margin = 20 * SCALE
    box = [b["x"] + margin, b["y"] + margin, b["x"] + b["width"] - margin, b["y"] + b["height"] - margin]
    draw.rounded_rectangle(box, radius=56 * SCALE, fill=CARD, outline=CARD_EDGE, width=3 * SCALE)

    lines = cell["lines"]
    inner = box[2] - box[0] - 2 * 44 * SCALE
    font = fitted(fonts, lines, (112 if len(lines) == 1 else 96) * SCALE, inner)
    line_height = font.size * 1.32
    icon_size = min(300 * SCALE, int((box[3] - box[1]) * 0.42))
    gap = 30 * SCALE
    block = icon_size + gap + line_height * len(lines)
    top = box[1] + (box[3] - box[1] - block) / 2
    centre = (box[0] + box[2]) / 2
    icon = ICONS[cell["action"]]
    draw_icon(image, fonts, icon, (centre, top + icon_size / 2), icon_size, colours["strong"])
    y = top + icon_size + gap + line_height / 2
    for line in lines:
        draw.text((centre, y), line, font=font, fill=TEXT, anchor="mm")
        y += line_height


def draw_menu(fonts, size, menu):
    width, height = size["width"] * SCALE, size["height"] * SCALE
    image = Image.new("RGB", (width, height), COLOURS[menu["tab"]]["light"])
    draw = ImageDraw.Draw(image)
    if menu["tabs"]:
        bar = max(tab["bounds"]["height"] for tab in menu["tabs"]) * SCALE
        draw.rectangle([0, 0, width - 1, bar - 1], fill=CARD)
        for tab in menu["tabs"]:
            draw_tab(image, draw, fonts, tab)
    for cell in menu["cells"]:
        draw_cell(image, draw, fonts, cell, menu["tab"])
    return image.resize((size["width"], size["height"]), Image.Resampling.LANCZOS)


def draw_placeholder(fonts):
    """A product card's photo when it has none: an image icon over ไม่มีรูปสินค้า, on grey, as
    large as the cards show photos (20:13)."""
    width, height = PLACEHOLDER["width"] * SCALE, PLACEHOLDER["height"] * SCALE
    image = Image.new("RGB", (width, height), IDLE_TAB)
    draw = ImageDraw.Draw(image)
    font = fonts.text(64 * SCALE)
    icon_size = 240 * SCALE
    gap = 24 * SCALE
    line_height = font.size * 1.32
    top = (height - icon_size - gap - line_height) / 2
    draw_icon(image, fonts, "image", (width / 2, top + icon_size / 2), icon_size, PLACEHOLDER_INK)
    middle = top + icon_size + gap + line_height / 2
    draw.text((width / 2, middle), "ไม่มีรูปสินค้า", font=font, fill=PLACEHOLDER_INK, anchor="mm")
    return image.resize((PLACEHOLDER["width"], PLACEHOLDER["height"]), Image.Resampling.LANCZOS)


def main():
    if not features.check("raqm"):
        sys.exit("Pillow has no raqm, so it can't shape Thai text")
    fetch_fonts()
    fonts = Fonts()
    spec = json.loads((HERE / "menus.json").read_text())
    for menu in spec["menus"]:
        image = draw_menu(fonts, spec["size"], menu)
        path = HERE / menu["image"]
        image.save(path, optimize=True)
        print(f"{path.name}: {path.stat().st_size} bytes")
    path = HERE.parent / "placeholder.png"
    draw_placeholder(fonts).save(path, optimize=True)
    print(f"{path.name}: {path.stat().st_size} bytes")


if __name__ == "__main__":
    main()
