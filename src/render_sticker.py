"""Decode TGS once, stream bounded RGBA PNG frames to the TUI worker."""
import io
import math
import subprocess
import sys

output = sys.stdout.buffer
sys.stdout = sys.stderr  # Library diagnostics must never corrupt the PNG stream.

from lottie.exporters.svg import export_svg
from lottie.parsers.tgs import parse_tgs


animation = parse_tgs(sys.argv[1])
start = float(animation.in_point or 0)
end = float(animation.out_point or start + 1)
fps = float(animation.frame_rate or 60)
if not all(math.isfinite(value) for value in (start, end, fps)) or fps <= 0:
    raise ValueError("invalid sticker timeline")
duration = max(1 / fps, (end - start) / fps)
count = 1 if "--single" in sys.argv else max(1, min(60, math.ceil(duration * 10)))
for index in range(count):
    frame = min(end - 0.001, start + index * fps / 10)
    svg = io.BytesIO()
    export_svg(animation, svg, frame=frame, pretty=False)
    png = subprocess.run(
        ["rsvg-convert", "--width", "256", "--height", "256", "--keep-aspect-ratio"],
        input=svg.getvalue(), stdout=subprocess.PIPE, check=True,
    ).stdout
    output.write(png)
    output.flush()
