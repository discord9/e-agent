"""Offline renderer/structural check for the inline maid desktop pet.

Pillow rasterizes the inline SVG's paths/circles to a transparent 480x480
preview, with no browser, network, or extra dependency. Only path commands
actually used by the pet (M m C c s l H h v z) are implemented; any other
command fails loudly so the checker is updated when the SVG grows.
"""

from pathlib import Path
import re
import xml.etree.ElementTree as ET

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
PET_HTML = ROOT / "src/ui/pet.html"
OUTPUT = ROOT / ".petcheck/maid_render.png"
SIZE = 480
SCALE = SIZE / 90.0

TOKEN = re.compile(r"[MmLlHhVvCcSsQqTtAaZz]|-?\d*\.?\d+(?:[eE][-+]?\d+)?")


def cubic(p0, p1, p2, p3, steps=18):
    points = []
    for index in range(1, steps + 1):
        t = index / steps
        mt = 1 - t
        points.append((
            mt**3 * p0[0] + 3 * mt**2 * t * p1[0] + 3 * mt * t**2 * p2[0] + t**3 * p3[0],
            mt**3 * p0[1] + 3 * mt**2 * t * p1[1] + 3 * mt * t**2 * p2[1] + t**3 * p3[1],
        ))
    return points


def path_points(data):
    tokens = TOKEN.findall(data)
    index = 0
    command = None
    current = start = (0.0, 0.0)
    previous_control = None
    paths = []
    points = []

    def number():
        nonlocal index
        value = float(tokens[index])
        index += 1
        return value

    while index < len(tokens):
        token = tokens[index]
        if token.isalpha():
            command = token
            index += 1
            if command == "z":
                paths.append((points + [start], True))
                points = []
                current = start
                previous_control = None
                continue
        elif command is None or command in "Mm":
            raise ValueError(f"unexpected number after {command or 'start'} in path data: {data!r}")
        relative = command.islower()
        origin = current
        if command in "Mm":
            x, y = number(), number()
            current = (x + origin[0], y + origin[1]) if relative else (x, y)
            if points:
                paths.append((points, False))
            points = [current]
            start = current
        elif command in "Cc":
            values = [number() for _ in range(6)]
            if relative:
                values = [values[i] + origin[i % 2] for i in range(6)]
            points.extend(cubic(origin, values[:2], values[2:4], values[4:]))
            current, previous_control = values[4:], values[2:4]
        elif command == "s":
            values = [number() for _ in range(4)]
            values = [values[i] + origin[i % 2] for i in range(4)]
            p1 = (2 * origin[0] - previous_control[0], 2 * origin[1] - previous_control[1]) if previous_control else origin
            points.extend(cubic(origin, p1, values[:2], values[2:]))
            current, previous_control = values[2:], values[:2]
        elif command == "l":
            x, y = number(), number()
            current = (x + origin[0], y + origin[1])
            points.append(current)
        elif command in "Hh":
            x = number() + (origin[0] if relative else 0)
            current = (x, origin[1])
            points.append(current)
        elif command == "v":
            y = number() + origin[1]
            current = (origin[0], y)
            points.append(current)
        else:
            raise ValueError(f"unsupported SVG path command: {command}")
        if command.upper() not in "CS":
            previous_control = None
    if points:
        paths.append((points, False))
    return paths


def color(value, opacity=1.0):
    if not value or value == "none":
        return None
    value = value.lstrip("#")
    if len(value) == 3:
        value = "".join(char * 2 for char in value)
    return tuple(int(value[i:i + 2], 16) for i in range(0, 6, 2)) + (round(255 * opacity),)


def render_node(node, image, inherited=None):
    inherited = inherited or {}
    style = inherited | node.attrib
    opacity = float(style.get("opacity", "1"))
    fill = color(style.get("fill", "#000000"), opacity)
    stroke = color(style.get("stroke"), opacity)
    width = max(1, round(float(style.get("stroke-width", "1")) * SCALE))
    draw = ImageDraw.Draw(image)
    tag = node.tag.split("}")[-1]

    if tag == "path":
        for points, closed in path_points(node.attrib["d"]):
            scaled = [(round(x * SCALE), round(y * SCALE)) for x, y in points]
            if fill and closed:
                draw.polygon(scaled, fill=fill)
            if stroke and len(scaled) > 1:
                draw.line(scaled, fill=stroke, width=width, joint="curve")
    elif tag == "circle":
        cx, cy, radius = (float(node.attrib[key]) for key in ("cx", "cy", "r"))
        box = tuple(round(value * SCALE) for value in (cx - radius, cy - radius, cx + radius, cy + radius))
        draw.ellipse(box, fill=fill, outline=stroke, width=width)

    children_style = {key: style[key] for key in ("fill", "stroke", "stroke-width", "opacity") if key in style}
    for child in node:
        render_node(child, image, children_style)


def main():
    source = PET_HTML.read_text(encoding="utf-8")
    matches = re.findall(r"<svg\b[^>]*>.*?</svg>", source, re.S)
    assert len(matches) == 1, "expected exactly one inline SVG"
    svg = ET.fromstring(matches[0])
    view_box = svg.attrib.get("viewbox") or svg.attrib.get("viewBox")
    assert view_box == "0 0 90 90", f"unexpected viewBox: {view_box}"
    expected = {"maid-silhouette", "maid-hair-back", "maid-headpiece", "maid-face", "maid-expression", "maid-uniform", "maid-accents"}
    groups = {node.attrib["id"] for node in svg.iter() if "id" in node.attrib}
    assert expected <= groups, f"missing semantic groups: {expected - groups}"
    assert "<script src=" not in source and "<image" not in source, "pet must remain local and inline"
    assert "M23.748 4.651" not in source, "legacy whale path leaked into active pet"

    image = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    render_node(svg, image)
    alpha = image.getchannel("A")
    bbox = alpha.getbbox()
    assert bbox is not None
    opaque = sum(1 for value in alpha.getdata() if value)
    assert opaque < SIZE * SIZE * 0.8, "preview background is not transparent"
    image.save(OUTPUT)
    print(f"rendered: {OUTPUT.relative_to(ROOT)} {image.size} {image.mode}")
    print(f"alpha bbox: {bbox}; visible pixels: {opaque}; groups: {len(groups)}")


if __name__ == "__main__":
    main()
