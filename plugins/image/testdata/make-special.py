#!/usr/bin/env python3
"""Write the hand-made test images (run from this folder; standard library only).

- rotated.jpg: tiny.jpg with an EXIF APP1 segment saying orientation 6
  (rotate 90° clockwise to display), so the decoded image is 32×48.
- bomb.png: a 60000×60000 PNG header (3.6 gigapixels) with a few bytes of
  image data: a decompression bomb the plugin must refuse before decoding.

The other images were made with ImageMagick and cwebp:
  magick -size 40x30 gradient:'#ff000000-#0000ffff' -define png:color-type=6 -strip tiny.png
  magick -seed 3 -size 48x32 plasma: -strip -quality 85 tiny.jpg
  magick -seed 4 -size 30x20 plasma: -colors 16 -strip tiny.gif
  magick -seed 5 -size 36x24 plasma: -strip ppm:- | cwebp -quiet -q 75 -o tiny-lossy.webp -- -
"""
import struct
import zlib

jpg = open("tiny.jpg", "rb").read()
assert jpg[:2] == b"\xff\xd8"
# TIFF header (big-endian), one IFD with one entry: Orientation (0x0112), SHORT, 1, value 6.
tiff = b"MM\x00\x2a" + struct.pack(">I", 8) + struct.pack(">H", 1) + struct.pack(">HHIHH", 0x0112, 3, 1, 6, 0) + struct.pack(">I", 0)
app1 = b"Exif\x00\x00" + tiff
open("rotated.jpg", "wb").write(jpg[:2] + b"\xff\xe1" + struct.pack(">H", len(app1) + 2) + app1 + jpg[2:])


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)


ihdr = struct.pack(">IIBBBBB", 60000, 60000, 8, 6, 0, 0, 0)
idat = zlib.compress(b"\x00" * 1024)
open("bomb.png", "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b""))
