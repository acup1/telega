"""Disabled reference renderer; the application uses ui/latex.rs and never invokes this script."""
import io
import sys

output = sys.stdout.buffer
sys.stdout = sys.stderr
import matplotlib
matplotlib.use("Agg")
from matplotlib.font_manager import FontProperties
from matplotlib.mathtext import math_to_image
from PIL import Image

source = sys.stdin.read(16384).strip()
if r"\begin{document}" in source:
    source = source.split(r"\begin{document}", 1)[1].split(r"\end{document}", 1)[0].strip()
for left, right in [("$$", "$$"), ("$", "$"), (r"\[", r"\]"), (r"\(", r"\)")]:
    if source.startswith(left) and source.endswith(right):
        source = source[len(left):-len(right)].strip()
for environment in ("equation", "equation*", "align", "align*", "aligned", "gather", "gather*"):
    source = source.replace(r"\begin{" + environment + "}", "").replace(r"\end{" + environment + "}", "")
source = source.replace(r"\displaystyle", "").replace("&", "").replace(r"\\", "\n")
frames = []
for line in source.splitlines():
    if not line.strip():
        continue
    buffer = io.BytesIO()
    math_to_image("$" + line.strip() + "$", buffer, prop=FontProperties(size=18), dpi=140,
                  format="png", color=sys.argv[1])
    frames.append(Image.open(buffer).convert("RGBA"))
if not frames:
    raise ValueError("empty formula")
canvas = Image.new("RGBA", (max(frame.width for frame in frames) + 16,
                            sum(frame.height + 8 for frame in frames) + 8))
y = 8
for frame in frames:
    canvas.alpha_composite(frame, ((canvas.width - frame.width) // 2, y))
    y += frame.height + 8
canvas.thumbnail((2048, 2048), Image.Resampling.LANCZOS)
canvas.save(output, format="PNG")
