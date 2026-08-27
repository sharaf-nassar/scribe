# Shared painted tab-strip geometry detector for visual E2E targets.

band_ink_width() {
    convert "$1" -colorspace Gray -threshold 12% -fuzz 5% -trim +repage -format "%w" info: 2>/dev/null
}
