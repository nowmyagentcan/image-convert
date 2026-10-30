#!/bin/sh
# Writes the fixtures that need an outside encoder; run on Ubuntu from this folder:
#   apt install imagemagick libjpeg-turbo-progs python3-pil
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
