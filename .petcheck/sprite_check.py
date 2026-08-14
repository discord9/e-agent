"""Offline structural/frame-math check for the configurable desktop pet.

No external sprite is copied into the repository. This checks the same
row-major CSS background-position math used by pet.html for the documented
1536x2288, 8x9 sheet and confirms all 72 frames stay in bounds.
"""
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PET_HTML = (ROOT / "src/ui/pet.html").read_text()
COLS, ROWS = 8, 9
FRAME_W, FRAME_H = 192, 254
SHEET_W, SHEET_H = 1536, 2288
assert SHEET_W == COLS * FRAME_W
# The supplied reference is two pixels taller than 9×254. CSS
# `background-size: 800% 900%` intentionally fits it to the configured frame
# grid (effective 1536×2286), exactly matching the browser implementation.
assert SHEET_H == ROWS * FRAME_H + 2
EFFECTIVE_W, EFFECTIVE_H = COLS * FRAME_W, ROWS * FRAME_H

positions = []
for frame in range(COLS * ROWS):
    col, row = frame % COLS, frame // COLS
    x_pct = 0 if COLS == 1 else col * 100 / (COLS - 1)
    y_pct = 0 if ROWS == 1 else row * 100 / (ROWS - 1)
    x_px = round(x_pct * (EFFECTIVE_W - FRAME_W) / 100)
    y_px = round(y_pct * (EFFECTIVE_H - FRAME_H) / 100)
    assert (x_px, y_px) == (col * FRAME_W, row * FRAME_H)
    positions.append((x_px, y_px))
assert len(set(positions)) == 72
assert positions[0] == (0, 0)
assert positions[7] == (1344, 0)
assert positions[8] == (0, 254)
assert positions[-1] == (1344, 2032)

required = [
    "frame = (frame + 1) % total",
    "col * 100 / (cols - 1)",
    "row * 100 / (rows - 1)",
    'Math.max(16, loopMs / total)',
    'backgroundSize = (cols * 100) + "% " + (rows * 100) + "%"',
]
for text in required:
    assert text in PET_HTML, f"missing runtime frame-step expression: {text}"

# The repository UI provides only an initially hidden mount point. A loaded
# configured sprite reveals it, including on narrow screens; no artwork or
# mobile-hide rule is bundled.
for forbidden in ("<svg", "maid-silhouette", ".pet-whale-svg", ".pet-body", "max-width: 480px"):
    assert forbidden not in PET_HTML, f"bundled artwork/mobile hide leaked: {forbidden}"
assert 'class="pet-whale"' in PET_HTML and "hidden>" in PET_HTML
assert "turn.replaceChildren(sprite)" in PET_HTML
assert "fish.hidden = false" in PET_HTML
print("verified 72 row-major frames and sprite-only hidden mount; mobile remains enabled")
