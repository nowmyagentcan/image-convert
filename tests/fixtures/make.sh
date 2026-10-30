#!/bin/sh
# Writes the fixtures that need an outside encoder; run on Ubuntu from this folder:
#   apt install imagemagick libjpeg-turbo-progs python3-pil colord-data
set -eu
cd "$(dirname "$0")"
im="convert -strip -define png:exclude-chunks=date,time"

# Adam7-interlaced PNG and the same pixels without interlacing.
$im -size 9x7 gradient:red-blue -interlace PNG png24:png-adam7.png
$im png-adam7.png -interlace none png24:png-adam7-plain.png

# Arithmetic-coded JPEG (SOF9).
convert -size 16x16 gradient:white-black ppm:- | cjpeg -arithmetic -quality 90 > jpeg-arithmetic.jpg

# CMYK JPEGs with an Adobe APP14 marker, ink 10/60/120/200, from two encoders.
python3 -c "from PIL import Image; Image.new('CMYK', (8, 8), (10, 60, 120, 200)).save('jpeg-cmyk-pil.jpg', quality=100)"
convert -size 8x8 'xc:cmyk(10,60,120,200)' -strip -quality 100 jpeg-cmyk-im.jpg

# 24 colour patches of 8x8 px with an embedded profile, and the sRGB that lcms2 renders from the
# decoded pixels: an Adobe RGB PNG (colord's "Compatible with Adobe RGB (1998)", CC0) and a CMYK
# JPEG (colord's SWOP TR005 coated, CC0, from CGATS TR 005 data). The reference uses relative
# colorimetric: this profile's perceptual table is the same, and lcms2's perceptual intent adds
# black point compensation towards its v4 sRGB, which moxcms doesn't do.
python3 - <<'PY'
from PIL import Image, ImageCms
icc = '/usr/share/color/icc/colord/'
srgb = ImageCms.createProfile('sRGB')
def patches(mode, colours):
    im = Image.new(mode, (48, 32))
    for i, c in enumerate(colours):
        x, y = i % 6 * 8, i // 6 * 8
        im.paste(c, (x, y, x + 8, y + 8))
    return im
def reference(im, profile, name):
    ImageCms.profileToProfile(im, profile, srgb, renderingIntent=1, outputMode='RGB').save(name)
adobe = icc + 'AdobeRGB1998.icc'
rgb = patches('RGB', [(r, g, b) for r in (60, 130, 200) for g in (70, 150) for b in (50, 110, 170, 230)])
rgb.save('color-adobergb.png', icc_profile=open(adobe, 'rb').read())
reference(rgb, adobe, 'color-adobergb-srgb.png')
swop = icc + 'SWOP_TR005_coated_5.icc'
inks = [(c, m, y, k) for c in (0, 180) for m in (30, 200) for y in (0, 90, 230) for k in (0, 60)]
patches('CMYK', inks).save('color-swop.jpg', quality=100, icc_profile=open(swop, 'rb').read())
reference(Image.open('color-swop.jpg'), swop, 'color-swop-srgb.png')
PY

# JPEG XL from cjxl 0.7 (apt install libjxl-tools), lossy with djxl's decode as the reference.
convert -seed 2 -size 64x48 plasma: -depth 8 png24:jxl-src.png
cjxl -q 90 jxl-src.png jxl-lossy.jxl
djxl jxl-lossy.jxl jxl-lossy-ref.png
cjxl -d 0 jxl-src.png jxl-lossless.jxl
convert jxl-src.png -colorspace Gray -depth 16 png:jxl-grey16.png
cjxl -d 0 jxl-grey16.png jxl-grey16.jxl
convert jxl-src.png \( +clone -fx 'i/w' -colorspace Gray \) -alpha off -compose CopyOpacity -composite png32:jxl-alpha.png
cjxl -d 0 jxl-alpha.png jxl-alpha.jxl
convert -delay 10 jxl-src.png \( jxl-src.png -negate \) gif:jxl-anim.gif
cjxl -d 0 jxl-anim.gif jxl-anim.jxl
convert "jxl-anim.gif[0]" png24:jxl-anim-ref.png
rm jxl-anim.gif
# CMYK with alpha: jxl-cmyk.jxl and jxl-cmyk-ref.png are input.jxl and ref.png of the libjxl
# conformance case testcases/cmyk_layers (github.com/libjxl/conformance, BSD-3-Clause: LICENSE-libjxl).
