"""Offline structural/frame-math check for the configurable desktop pet.

No external sprite is copied into the repository. This checks the same
single-row CSS background-position math used by pet.html for the documented
1536x2288, 8x11 sheet.
"""
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PET_HTML = (ROOT / "src/ui/pet.html").read_text()
COLS, ROWS = 8, 11
FRAME_W, FRAME_H = 192, 208
SHEET_W, SHEET_H = 1536, 2288
IDLE_ROW, IDLE_FRAMES = 0, 6
assert SHEET_W == COLS * FRAME_W
# The reference is exactly 11 rows of 208 px: 11 x 208 = 2288. CSS
# `background-size: 800% 1100%` maps the frame grid onto the sheet 1:1.
assert SHEET_H == ROWS * FRAME_H
EFFECTIVE_W, EFFECTIVE_H = COLS * FRAME_W, ROWS * FRAME_H

positions = []
frame = 0
for _ in range(IDLE_FRAMES + 1):
    col = frame % COLS
    x_pct = 0 if COLS == 1 else col * 100 / (COLS - 1)
    y_pct = 0 if ROWS == 1 else IDLE_ROW * 100 / (ROWS - 1)
    x_px = round(x_pct * (EFFECTIVE_W - FRAME_W) / 100)
    y_px = round(y_pct * (EFFECTIVE_H - FRAME_H) / 100)
    assert (x_px, y_px) == (col * FRAME_W, IDLE_ROW * FRAME_H)
    positions.append((x_px, y_px))
    frame = (frame + 1) % IDLE_FRAMES

# Row 0 advances through exactly its six real frames, then wraps to col 0.
assert positions[:6] == [(col * FRAME_W, 0) for col in range(6)]
assert positions[6] == (0, 0)
assert all(y == 0 for _, y in positions)

# A shorter state row also loops before its padded empty cells.
frame = 0
short_row_cols = []
for _ in range(6):
    short_row_cols.append(frame % COLS)
    frame = (frame + 1) % 5
assert short_row_cols == [0, 1, 2, 3, 4, 0]

required = [
    "frame = (frame + 1) % idleFrames",
    "const col = frame % cols",
    "idleRow * 100 / (rows - 1)",
    'Math.max(16, loopMs / idleFrames)',
    'backgroundSize = (cols * 100) + "% " + (rows * 100) + "%"',
]
for text in required:
    assert text in PET_HTML, f"missing single-row frame-step expression: {text}"

for forbidden in (
    "const total = cols * rows",
    "Math.floor(frame / cols)",
    "frame = (frame + 1) % total",
):
    assert forbidden not in PET_HTML, f"whole-sheet stepping leaked: {forbidden}"

# The repository UI provides only an initially hidden mount point. A loaded
# configured sprite reveals it, including on narrow screens; no artwork or
# mobile-hide rule is bundled.
for forbidden in ("<svg", "maid-silhouette", ".pet-whale-svg", ".pet-body", "max-width: 480px"):
    assert forbidden not in PET_HTML, f"bundled artwork/mobile hide leaked: {forbidden}"
assert 'class="pet-whale"' in PET_HTML and "hidden>" in PET_HTML
assert "image.onload" in PET_HTML and "turn.replaceChildren(sprite)" in PET_HTML
assert "fish.hidden = false" in PET_HTML
assert 'sprite.setAttribute("role", "img")' in PET_HTML
assert 'sprite.setAttribute("aria-label", "桌宠动画")' in PET_HTML
assert 'aria-label="关闭桌宠"' in PET_HTML and ".pet-close:focus-visible" in PET_HTML
assert '@media (prefers-reduced-motion: reduce)' in PET_HTML
assert '!window.matchMedia("(prefers-reduced-motion: reduce)").matches' in PET_HTML
print("verified fixed-row idle stepping, frame-count wrap, preload/reduced-motion/accessibility, and mobile-visible mount")
