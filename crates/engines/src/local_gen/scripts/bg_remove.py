"""Remove a picture's background (rembg with BiRefNet) for Craft Anything's local generation.

usage: python bg_remove.py <in.png> <out_rgba.png> <out_on_white.png>
Writes the subject on a transparent background, and the same composited on white (what the 3D
model is given), then prints "BGREMOVE ok".
"""
import sys

from PIL import Image
from rembg import new_session, remove

src, out_rgba, out_white = sys.argv[1:4]
img = Image.open(src).convert("RGBA")
cut = remove(img, session=new_session("birefnet-general"))
cut.save(out_rgba)
white = Image.new("RGBA", cut.size, (255, 255, 255, 255))
white.alpha_composite(cut)
white.convert("RGB").save(out_white)
print("BGREMOVE ok")
