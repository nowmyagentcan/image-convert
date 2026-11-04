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

# One colour, its chroma sampled more finely than its luma (Cb 2x1, then Cr 2x2).
convert -size 53x37 'xc:rgb(80,160,220)' ppm:- | cjpeg -sample 1x1,2x1,1x1 -quality 95 > jpeg-chroma-cb2x1.jpg
convert -size 53x37 'xc:rgb(80,160,220)' ppm:- | cjpeg -sample 1x1,1x1,2x2 -quality 95 > jpeg-chroma-cr2x2.jpg

# Progressive with uneven chroma, a layout jpeg-decoder can't follow on this gradient.
convert -size 53x37 gradient:'#102030-#f0e0d0' ppm:- | cjpeg -sample 1x2,2x1,1x4 -progressive -quality 90 > jpeg-progressive-uneven.jpg

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

# TIFF variants from libtiff and ImageMagick, each with ImageMagick's decode as the reference:
#   apt install libtiff-tools
convert -seed 1 -size 40x30 plasma: -depth 8 png24:tiff-src.png
convert tiff-src.png -define tiff:endian=msb -compress LZW -define tiff:predictor=2 tiff-be-lzw.tif
convert tiff-src.png -compress None tiff-plain.tif
tiffcp -c jpeg:r:90 tiff-plain.tif tiff-rgb-jpeg.tif
tiffcp -c jpeg:90 tiff-plain.tif tiff-ycbcr-jpeg.tif
convert tiff-src.png -colorspace Gray -threshold 50% -type Bilevel -define tiff:photometric=min-is-white -compress Group4 tiff-g4.tif
convert tiff-src.png -colorspace Gray -threshold 50% -type Bilevel -depth 1 -compress None -define tiff:photometric=min-is-black tiff-bilevel.tif
convert tiff-src.png -colorspace Gray -depth 8 -compress Zip -define tiff:photometric=min-is-white tiff-white.tif
# Grey + alpha, which ImageMagick 6 writes as RGBA: 8-bit PackBits from Pillow, and 16-bit
# big-endian uncompressed written field by field; both come back as tiff-greya8-ref.png.
python3 - <<'PY'
import struct
from PIL import Image
la = Image.open('tiff-src.png').convert('LA')
la.putalpha(Image.linear_gradient('L').resize(la.size))
la.save('tiff-greya8.tif', compression='packbits')
la.save('tiff-greya8-ref.png')
w, h = la.size
px = b''.join(struct.pack('>HH', g * 257, a * 257 + (a < 255)) for g, a in la.getdata())
tags = [(256, 3, 1, w), (257, 3, 1, h), (258, 3, 1, 16), (259, 3, 1, 1), (262, 3, 1, 1),
        (273, 4, 1, 0), (277, 3, 1, 2), (278, 3, 1, h), (279, 4, 1, len(px)), (338, 3, 1, 2)]
start = 8 + 2 + 12 * len(tags) + 4
ifd = struct.pack('>H', len(tags)) + b''.join(
    struct.pack('>HHI', t, k, n) + (struct.pack('>HH', v, 0) if k == 3 else
                                     struct.pack('>I', start if t == 273 else v))
    for t, k, n, v in tags) + b'\0\0\0\0'
open('tiff-greya16.tif', 'wb').write(b'MM\0*\0\0\0\x08' + ifd + px)
PY
rm tiff-plain.tif
for f in tiff-rgb-jpeg tiff-ycbcr-jpeg tiff-g4 tiff-bilevel tiff-white; do
  convert $f.tif -depth 8 png:$f-ref.png
done

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

# HEIC and AVIF from heif-enc 1.17 for the OS decoders, lossless unless named otherwise:
#   apt install libheif-examples libheif-plugin-x265 libheif-plugin-aomenc
# heif-irot.*: a JPEG with EXIF orientation 6, which heif-enc turns into irot; the EXIF item's
# orientation is then set back to 6, so a decoder that also applied it would rotate twice.
# heif-irot-ref.png is the JPEG's pixels upright. heif-p3.avif holds the Display P3 patches of
# tests/pipeline.rs under nclx code points 12/13/0 (heif-p3-yuv.avif: the same as lossy YCbCr under
# 12/13/1); heif-adobergb.heic the Adobe RGB patches
# with their ICC profile; heif-alpha.avif an alpha gradient; heif-10bit.avif a 16-bit gradient,
# lossy YCbCr (WIC misreads 10-bit samples under the identity matrix).
lossless="-L -p chroma=444 --matrix_coefficients=0"
convert -seed 3 -size 128x64 plasma: -depth 8 ppm:- | cjpeg -quality 100 > heif-src.jpg
python3 - <<'PY'
from PIL import Image
im = Image.open('heif-src.jpg')
exif = Image.Exif()
exif[0x0112] = 6
im.save('heif-o6.jpg', quality=100, subsampling=0, exif=exif.tobytes())
Image.open('heif-o6.jpg').transpose(Image.Transpose.ROTATE_270).save('heif-irot-ref.png')
colours = [(r, g, b) for r in (40, 120, 210) for g in (60, 180) for b in (50, 140, 220)]
p3 = Image.new('RGB', (48, 24))
for i, c in enumerate(colours):
    p3.paste(c, (i % 6 * 8, i // 6 * 8, i % 6 * 8 + 8, i // 6 * 8 + 8))
p3.save('heif-p3.png')
PY
heif-enc $lossless heif-o6.jpg -o heif-irot.heic
heif-enc -A $lossless heif-o6.jpg -o heif-irot.avif
python3 - <<'PY'
import struct
def u32(d, at):
    return struct.unpack('>I', d[at:at + 4])[0]
def child(d, start, end, kind):
    while start < end:
        if d[start + 4:start + 8] == kind:
            return start
        start += u32(d, start)
    raise KeyError(kind)
# heif-enc's AV1 path writes no irot, so add one (270 degrees anticlockwise, essential) to item 1
# and move everything after it, fixing the sizes of its parents and the offsets in iloc.
d = bytearray(open('heif-irot.avif', 'rb').read())
meta = child(d, 0, len(d), b'meta')
iprp = child(d, meta + 12, meta + u32(d, meta), b'iprp')
ipco = child(d, iprp + 8, iprp + u32(d, iprp), b'ipco')
ipma = child(d, iprp + 8, iprp + u32(d, iprp), b'ipma')
iloc = child(d, meta + 12, meta + u32(d, meta), b'iloc')
assert d[ipma + 8:ipma + 18] == bytes.fromhex('00000000 00000001 0001') and d[iloc + 8:iloc + 14] == bytes.fromhex('00000000 4440')
assert ipco < ipma
props, at = 0, ipco + 8
while at < ipco + u32(d, ipco):
    props, at = props + 1, at + u32(d, at)
count = d[ipma + 18]
d[ipma + 19 + count:ipma + 19 + count] = bytes([0x80 | (props + 1)])
d[ipma + 18] = count + 1
d[at:at] = struct.pack('>I', 9) + b'irot' + bytes([3])
for box, grow in ((meta, 10), (iprp, 10), (ipco, 9), (ipma + 9, 1)):
    d[box:box + 4] = struct.pack('>I', u32(d, box) + grow)
at = iloc + 14 + 2
for _ in range(struct.unpack('>H', d[iloc + 14:iloc + 16])[0]):
    extents = struct.unpack('>H', d[at + 8:at + 10])[0]
    for e in range(extents):
        off = at + 10 + e * 8
        d[off:off + 4] = struct.pack('>I', u32(d, off) + 10)
    at += 10 + extents * 8
open('heif-irot.avif', 'wb').write(d)
# Sets the EXIF Orientation (IFD0 tag 274) inside each file back to 6.
for name in ('heif-irot.heic', 'heif-irot.avif'):
    data = bytearray(open(name, 'rb').read())
    tag = data.find(b'\x01\x12\x00\x03\x00\x00\x00\x01')
    if tag < 0:
        tag = data.find(b'\x12\x01\x03\x00\x01\x00\x00\x00')
        data[tag + 8:tag + 10] = b'\x06\x00'
    else:
        data[tag + 8:tag + 10] = b'\x00\x06'
    open(name, 'wb').write(data)
PY
heif-enc -A $lossless --colour_primaries=12 --transfer_characteristic=13 heif-p3.png -o heif-p3.avif
heif-enc -A -q 100 -p chroma=444 --matrix_coefficients=1 --colour_primaries=12 --transfer_characteristic=13 heif-p3.png -o heif-p3-yuv.avif
heif-enc $lossless color-adobergb.png -o heif-adobergb.heic
convert tiff-src.png \( +clone -fx 'i/w' -colorspace Gray \) -alpha off -compose CopyOpacity -composite png32:heif-alpha.png
heif-enc -A $lossless heif-alpha.png -o heif-alpha.avif
convert -size 64x32 gradient:'#102030-#f0e0d0' -depth 16 png48:heif-10bit.png
heif-enc -A -q 95 -p chroma=444 -b 10 heif-10bit.png -o heif-10bit.avif
rm heif-src.jpg heif-o6.jpg heif-p3.png
