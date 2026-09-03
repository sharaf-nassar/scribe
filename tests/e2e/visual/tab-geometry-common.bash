# Shared painted tab-strip geometry detector for visual E2E targets.

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
measure_bar_height() {
    local image="$1" top="$2" scan_h="$3" label="$4"
    local x target_y target end mask
    x=$(( WIN_W - 4 ))
    target_y=$(( top + scan_h - 2 ))
    target=$(convert "$image" -format "%[pixel:p{$x,$target_y}]" info:)
    mask="/tmp/${label}-boundary-mask.png"
    convert "$image" -crop "1x${scan_h}+${x}+${top}" +repage -alpha on \
        -fuzz 2% -transparent "$target" "$mask" >/dev/null
    end=$(convert "$mask" -bordercolor none -border 1 -trim \
        -format '%[fx:page.y+h-1]' info: 2>/dev/null || true)
    [ -n "$end" ] || fail "$label row boundary was not measurable"
    printf '%s' "${end%.*}"
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
