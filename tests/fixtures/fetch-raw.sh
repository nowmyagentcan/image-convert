#!/bin/sh
# Downloads one CC0 sample of each RAW kind from raw.pixls.us into ${1:-target/raw-samples} (from
# the repo root) and checks each file's SHA-256. The ignored test real_raw_samples converts them:
#   sh tests/fixtures/fetch-raw.sh && cargo test --test decode real_raw_samples -- --ignored
set -eu
dir=${1:-target/raw-samples}
mkdir -p "$dir"
cd "$dir"
base=https://raw.pixls.us/getfile.php
while read -r sha name path; do
  if ! echo "$sha  $name" | sha256sum -c --status 2>/dev/null; then
    curl -fsSL --retry 5 --retry-all-errors -o "$name" "$base/$path"
    echo "$sha  $name" | sha256sum -c --quiet
  fi
done <<'LIST'
12402b46f3cfaa66a671c7b6524c741dd17d6a2add3bc6cc41dcaf1570b0e722 canon-eos-kiss-f.cr2 6457/nice/Canon%20-%20EOS%20Kiss%20F%20-%20RAW%20(3:2).CR2
74abb0a113d075ad9887a058082f40dd2a938c4813a08474d82356f11a027778 canon-eos-r6.cr3 4659/nice/Canon%20-%20EOS%20R6%20-%203:2.CR3
9e05c61fd5b02dc11233e45b4f1e3f6d60ed2ccd0b72f9e7cc487b76f3a9ba72 nikon-d500.nef 2506/nice/Nikon%20-%20D500%20-%2012bit%2012bit%20compressed%20(Lossless)%20(3:2).NEF
7cc338a0abc8fdad32d61006f1f8f412297b93e040c7efba572eb2bd8f8e8be2 sony-ilce-7s.arw 1581/nice/Sony%20-%20ILCE-7S%20-%2014bit%2014bit%20compressed%20(3:2).ARW
50bc64ee8cfbbfb6c0531e29ab428ff985506a16fe2d1d2e74656f21b54505f6 apple-iphone-xs.dng 3372/nice/Apple%20-%20iPhone%20XS%20-%2016bit%20(4:3).dng
519e1ac6cedd0424c17896c4e5ba40810796927baf536055d39623996894c16f fujifilm-x-s10.raf 4189/nice/Fujifilm%20-%20X-S10%20-%2014bit%2014bit%20compressed%20(3:2).RAF
abefe8e2cc8b42820764990b4ba94858f8c032fb38a2b9305bcd0af4d74ba099 panasonic-dmc-lx7.rw2 1771/nice/Panasonic%20-%20DMC-LX7%20-%201:1.RW2
dd0d7b76d89039995d253adddcfb44d131414fad2179610d9ea4d8d4bb876b7b pentax-k-r.pef 8171/nice/Pentax%20-%20K-r%20-%2012bit%20compressed%20(3:2).PEF
LIST
