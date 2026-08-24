# -*- coding: utf-8 -*-
"""Generate the installer executable icon (icon.ico) for the Tauri shell
from 256x256.png with all sizes Windows needs (16/24/32/48/64/128/256).
"""
import os
from PIL import Image

BASE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.join(BASE, "src-tauri", "icons", "256x256.png")
OUT_ICO = os.path.join(BASE, "src-tauri", "icons", "icon.ico")

img = Image.open(SRC)
print(f"source: mode={img.mode} size={img.size}")

ico_sizes = [256, 128, 64, 48, 32, 24, 16]
if img.mode != "RGBA":
    img = img.convert("RGBA")
img.save(OUT_ICO, format="ICO", sizes=[(s, s) for s in ico_sizes])
print(f"icon.ico written ({os.path.getsize(OUT_ICO)} bytes, sizes={ico_sizes})")