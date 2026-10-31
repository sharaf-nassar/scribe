# Shared painted tab-strip geometry detector for visual E2E targets.

# The client's scale factor, so suites written in logical pixels drive the same
# layout on a HiDPI X11 session (`GPUI_X11_SCALE_FACTOR=2`): xdotool, xwininfo
# and screenshots all speak device pixels. Whole factors only.
E2E_SCALE="${GPUI_X11_SCALE_FACTOR:-1}"
case "$E2E_SCALE" in
    1 | 2 | 3) ;;
    *) echo "FATAL: GPUI_X11_SCALE_FACTOR must be 1, 2 or 3 for the E2E geometry, got $E2E_SCALE" >&2; exit 2 ;;
esac
# A logical length in device pixels.
dev() { echo $(( $1 * E2E_SCALE )); }

band_ink_width() {
    convert "$1" -colorspace Gray -threshold 12% -fuzz 5% -trim +repage -format "%w" info: 2>/dev/null
}

# Measure the bottom of the chrome marks that differ from the stable pane
# background sampled at the end of this scan, as an offset from `top`. Titlebar
# and grid backgrounds can be identical; the tab card still ends exactly at the
# row boundary. The right edge carries no terminal glyph ink below it.
#
# The mask is bordered with transparency before the trim. `-trim` removes edges
# matching the TOP-LEFT pixel, so a scan whose first row is already a chrome
# mark (rather than background) made trim treat the mark as the border and
# collapse the whole strip to nothing, reporting 0 instead of a height. That is
# what the strip looked like once the layered chrome dropped the hairline under
# the region bars: bar fill on top, ground below, no third colour. The added
# transparent border makes the corner background again, so the trim always
# yields the mark bounding box and an off-by-one `top` reports a wrong height
# instead of a silent zero.
#
# Tabs no longer reach the window's right edge (they cap at 230px inside a 6px
# run inset), the strip is the same ground tone as the gaps below it, and an
# open tab's card now runs unbroken through its bridge into the pane card, so
# no single fixed column reliably crosses the bar boundary. The boundary is
# taken from the pane card instead: every bar is followed by a card whose top
# edge sits exactly PANE_CARD_MARGIN (6px) of ground below it. Columns are
# sampled across the window; in each, the first non-ground row that follows a
# run of at least 6 ground rows is a candidate. The card top is the one row
# most columns agree on: a column under an open tab gives none, and glyphs or
# gaps give scattered rows, so the most common candidate wins (ties go deeper). Comparison is exact: the
# card's whisper edge differs from the ground by a single channel step.
# Reported as the old detector's "last bar row" offset from `top`.
measure_bar_height() {
    local image="$1" top="$2" scan_h="$3" label="$4"
    local end
    end=$(python3 - "$image" "$top" "$scan_h" "$WIN_W" "$E2E_SCALE" <<'PY'
import subprocess, sys

image, top, scan_h, width = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
scale = int(sys.argv[5])
# The E2E image carries ImageMagick, not PIL: read the band as text pixels.
raw = subprocess.run(
    ["convert", image, "-crop", f"{width}x{scan_h}+0+{top}", "+repage", "txt:-"],
    capture_output=True, text=True, check=True,
).stdout
px = {}
for line in raw.splitlines():
    if not line or line[0] == "#":
        continue
    pos, rest = line.split(":", 1)
    x, y = (int(v) for v in pos.split(","))
    body = rest.split("(", 1)[1].split(")", 1)[0]
    px[(x, y)] = tuple(int(float(v)) for v in body.split(",")[:3])
rows = 1 + max(y for _, y in px)
ground = px[(width - 2, 0)]
votes = {}
for x in range(10, width - 10, 6):
    run = 0
    for y in range(rows):
        if px.get((x, y)) == ground:
            run += 1
            continue
        if run >= 6 * scale:
            votes[y] = votes.get(y, 0) + 1
            break
        run = 0
if not votes:
    raise SystemExit(1)
card_top = max(votes, key=lambda y: (votes[y], y))
print(card_top - 6 * scale - 1)
PY
) || fail "$label row boundary was not measurable"
    printf '%s' "$end"
}

# Measure the status band against the stable grid background at the right edge.
# The top hairline starts the distinct run and the sampled terminal column has
# no glyph ink there, so its offset is the border-box band boundary.
measure_status_height() {
    local image="$1" top="$2" scan_h="$3" label="$4"
    local x target target_y mask start
    x=$(( WIN_W - 4 ))
    target_y=$(( top + 2 ))
    target=$(convert "$image" -format "%[pixel:p{$x,$target_y}]" info:)
    mask="/tmp/${label}-status-mask.png"
    convert "$image" -crop "1x${scan_h}+${x}+${top}" +repage -alpha on \
        -fuzz 2% -transparent "$target" "$mask" >/dev/null
    start=$(convert "$mask" -trim -format '%[fx:page.y]' info: 2>/dev/null || true)
    [ -n "$start" ] || fail "$label status boundary was not measurable"
    printf '%s' "$(( scan_h - ${start%.*} ))"
}
