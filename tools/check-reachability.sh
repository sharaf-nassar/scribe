#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  tools/check-reachability.sh --staged
  tools/check-reachability.sh --working-tree
  tools/check-reachability.sh --range <base> <head>

Ratchet the GPUI client's reachability surface against a committed baseline.

Reports how much of the crate the running binary can actually reach:
  * library modules imported on the live path of `scribe-client-gpui`
  * `ServerMessage` variants the live IPC reader acts on
  * `LayoutAction` variants the key path executes

Anything unreachable must be listed in tools/reachability-baseline.txt. The
check fails when the unreachable set grows, and also when a baseline entry has
become reachable, so the baseline can only ever shrink.
EOF
}

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "error: must run inside a git work tree" >&2
    exit 2
fi

if [[ $# -eq 0 ]]; then
    usage >&2
    exit 2
fi

mode=""
base_ref=""
head_ref=""
temp_dir=""
rows_file=""
found_file=""
expected_file=""
unexpected_file=""
stale_file=""

cleanup() {
    if [[ -n "$temp_dir" && -d "$temp_dir" ]]; then
        rm -rf "$temp_dir"
    fi
    rm -f "$rows_file" "$found_file" "$expected_file" "$unexpected_file" "$stale_file"
}
trap cleanup EXIT

case "$1" in
    --staged)
        if [[ $# -ne 1 ]]; then
            usage >&2
            exit 2
        fi
        mode="staged"
        ;;
    --working-tree)
        if [[ $# -ne 1 ]]; then
            usage >&2
            exit 2
        fi
        mode="working-tree"
        ;;
    --range)
        if [[ $# -ne 3 ]]; then
            usage >&2
            exit 2
        fi
        mode="range"
        base_ref="$2"
        head_ref="$3"
        ;;
    --help | -h)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac

repo_root="$(git rev-parse --show-toplevel)"

case "$mode" in
    staged)
        temp_dir="$(mktemp -d)"
        git checkout-index --all --prefix="$temp_dir/"
        scan_root="$temp_dir"
        context="the staged tree"
        ;;
    working-tree)
        scan_root="$repo_root"
        context="the working tree"
        ;;
    range)
        temp_dir="$(mktemp -d)"
        git archive "$head_ref" | tar -x -C "$temp_dir"
        scan_root="$temp_dir"
        context="the $head_ref tree"
        ;;
esac

baseline_file="$scan_root/tools/reachability-baseline.txt"
if [[ ! -f "$baseline_file" ]]; then
    echo "error: missing reachability baseline at $baseline_file" >&2
    exit 2
fi

rows_file="$(mktemp)"
found_file="$(mktemp)"
expected_file="$(mktemp)"
unexpected_file="$(mktemp)"
stale_file="$(mktemp)"

SCAN_ROOT="$scan_root" perl <<'PERL' >"$rows_file"
use strict;
use warnings;

my $root = $ENV{SCAN_ROOT} // die "SCAN_ROOT is required\n";
my $gpui = "$root/crates/scribe-client-gpui/src";

sub slurp {
    my ($path) = @_;
    open my $fh, '<', $path or die "open $path: $!\n";
    local $/;
    my $text = <$fh>;
    close $fh;
    return $text;
}

# Strip character literals, string literals and line comments so brace/paren
# counting is not thrown off by punctuation inside doc comments or format
# strings.
#
# Character literals go first and on their own: a `'"'` literal carries a lone
# double quote, which would otherwise open a string the string-literal pass
# then closes somewhere far away, swallowing whole declarations.
sub strip_noise {
    my ($text) = @_;
    $text =~ s{'(?:[^'\\]|\\.)'}{''}gs;
    $text =~ s{"(?:[^"\\]|\\.)*"}{""}gs;
    $text =~ s{//[^\n]*}{}g;
    return $text;
}

# Body of the first `fn <name>` in $text, without its enclosing braces.
sub fn_body {
    my ($text, $name) = @_;
    my $start = $text =~ /\bfn \Q$name\E\b/g ? pos($text) : undef;
    die "reachability: could not find fn $name\n" unless defined $start;
    my $open = index($text, '{', $start);
    die "reachability: could not find the body of fn $name\n" if $open < 0;
    my $depth = 0;
    for my $i ($open .. length($text) - 1) {
        my $ch = substr($text, $i, 1);
        $depth++ if $ch eq '{';
        if ($ch eq '}') {
            $depth--;
            return substr($text, $open + 1, $i - $open - 1) if $depth == 0;
        }
    }
    die "reachability: unbalanced body for fn $name\n";
}

# Variant names of `pub enum <name>`, in declaration order.
sub enum_variants {
    my ($text, $name) = @_;
    my ($body) = $text =~ /\bpub enum \Q$name\E\s*\{(.*?)\n\}/s;
    die "reachability: could not find pub enum $name\n" unless defined $body;
    my @variants = $body =~ /^    ([A-Z][A-Za-z0-9]*)\s*(?:\{|\(|,)/gm;
    die "reachability: no variants parsed from enum $name\n" unless @variants;
    return @variants;
}

# ── Library modules vs the binary's import closure ──────────────────────
my $lib = slurp("$gpui/lib.rs");
my @modules = $lib =~ /^pub mod (\w+);/gm;
die "reachability: no `pub mod` entries in lib.rs\n" unless @modules;
my %is_module = map { $_ => 1 } @modules;

my $main_src = slurp("$gpui/main.rs");
my @live_files = ('main.rs');
for my $submodule ($main_src =~ /^mod (\w+);/gm) {
    if (-f "$gpui/$submodule.rs") {
        push @live_files, "$submodule.rs";
    } elsif (-f "$gpui/$submodule/mod.rs") {
        push @live_files, "$submodule/mod.rs";
    } else {
        die "reachability: cannot resolve binary submodule $submodule\n";
    }
}

my $live = join "\n", map { slurp("$gpui/$_") } @live_files;
my %wired;
my $use_tree = qr/
    scribe_client_gpui::(?<tail>(?&BRACE)|\w+)
    (?(DEFINE)(?<BRACE>\{(?:[^{}]++|(?&BRACE))*\}))
/x;
while ($live =~ /$use_tree/g) {
    my $tail = $+{tail};
    $wired{$_} = 1 for grep { $is_module{$_} } ($tail =~ /([a-z][a-z0-9_]*)/g);
}
for my $module (@modules) {
    printf "module\t%s\t%s\n", $module, ($wired{$module} ? 'wired' : 'unwired');
}

# ── ServerMessage variants vs the live reader ───────────────────────────
my $protocol = strip_noise(slurp("$root/crates/scribe-common/src/protocol.rs"));
my @server_messages = enum_variants($protocol, 'ServerMessage');

my $main_code = strip_noise($main_src);
my $variant_table = fn_body($main_code, 'server_message_variant');
my %named_in_table = map { $_ => 1 } ($variant_table =~ /ServerMessage::(\w+)/g);
my @unnamed = grep { !$named_in_table{$_} } @server_messages;
die "reachability: server_message_variant does not name: @unnamed\n" if @unnamed;

my $reader = fn_body($main_code, 'dispatch_server_message');
my %handled_message = map { $_ => 1 } ($reader =~ /ServerMessage::(\w+)/g);
for my $variant (@server_messages) {
    printf "server-message\t%s\t%s\n", $variant,
        ($handled_message{$variant} ? 'handled' : 'unhandled');
}

# ── LayoutAction variants vs the key-path dispatcher ────────────────────
my $keybindings = strip_noise(slurp("$gpui/keybindings.rs"));
my @layout_actions = enum_variants($keybindings, 'LayoutAction');

my $dispatch = fn_body($main_code, 'handle_layout_action');
my %named_in_dispatch = map { $_ => 1 } ($dispatch =~ /LayoutAction::(\w+)/g);
my @unmatched = grep { !$named_in_dispatch{$_} } @layout_actions;
die "reachability: handle_layout_action does not name: @unmatched\n" if @unmatched;

my ($before) = $dispatch =~ /(.*?)=>\s*unhandled_layout_action/s;
die "reachability: handle_layout_action has no unhandled_layout_action arm\n"
    unless defined $before;
my ($chain) = $before =~ /((?:LayoutAction::\w+\s*\|\s*)*LayoutAction::\w+\s*)\z/s;
die "reachability: could not read the unhandled LayoutAction arm\n" unless defined $chain;
my %unhandled_action = map { $_ => 1 } ($chain =~ /LayoutAction::(\w+)/g);
for my $variant (@layout_actions) {
    printf "layout-action\t%s\t%s\n", $variant,
        ($unhandled_action{$variant} ? 'unhandled' : 'handled');
}
PERL

count_rows() {
    grep -cE "^$1	[^	]+	$2\$" "$rows_file" || true
}

modules_total="$(count_rows module '(wired|unwired)')"
modules_wired="$(count_rows module wired)"
messages_total="$(count_rows server-message '(handled|unhandled)')"
messages_handled="$(count_rows server-message handled)"
actions_total="$(count_rows layout-action '(handled|unhandled)')"
actions_handled="$(count_rows layout-action handled)"

echo "reachability: ${modules_wired}/${modules_total} modules wired, ${messages_handled}/${messages_total} server messages handled, ${actions_handled}/${actions_total} layout actions handled"

{
    awk -F '\t' '$1 == "module" && $3 == "unwired" { print "unwired-module " $2 }' "$rows_file"
    awk -F '\t' '$1 == "server-message" && $3 == "unhandled" { print "unhandled-server-message " $2 }' "$rows_file"
    awk -F '\t' '$1 == "layout-action" && $3 == "unhandled" { print "unhandled-layout-action " $2 }' "$rows_file"
} | sort >"$found_file"

grep -E '^(unwired-module|unhandled-server-message|unhandled-layout-action) ' "$baseline_file" \
    | sort >"$expected_file" || true

comm -23 "$found_file" "$expected_file" >"$unexpected_file" || true
comm -13 "$found_file" "$expected_file" >"$stale_file" || true

baseline_count() {
    awk -v key="$1" '$1 == "counts" && $2 == key { print $3 }' "$baseline_file"
}

count_errors=""
compare_count() {
    local key="$1" actual="$2" direction="$3"
    local expected
    expected="$(baseline_count "$key")"
    if [[ -z "$expected" ]]; then
        count_errors+="  missing baseline count: counts $key"$'\n'
        return
    fi
    if [[ "$expected" == "$actual" ]]; then
        return
    fi
    local verdict="changed"
    if [[ "$direction" == "up-is-progress" ]]; then
        verdict=$([[ "$actual" -lt "$expected" ]] && echo "REGRESSED" || echo "improved")
    else
        verdict=$([[ "$actual" -gt "$expected" ]] && echo "grew" || echo "shrank")
    fi
    count_errors+="  counts $key: baseline $expected, found $actual ($verdict)"$'\n'
}

compare_count modules-total "$modules_total" neutral
compare_count modules-wired "$modules_wired" up-is-progress
compare_count server-messages-total "$messages_total" neutral
compare_count server-messages-handled "$messages_handled" up-is-progress
compare_count layout-actions-total "$actions_total" neutral
compare_count layout-actions-handled "$actions_handled" up-is-progress

if [[ ! -s "$unexpected_file" && ! -s "$stale_file" && -z "$count_errors" ]]; then
    exit 0
fi

{
    echo "GPUI client reachability drifted from the baseline in $context."
    echo
    echo "The baseline records what the shipped binary cannot reach today."
    echo "It may only shrink: wire the surface up, or, if a new unreachable"
    echo "surface is genuinely intended, add it to tools/reachability-baseline.txt"
    echo "deliberately and say which bead will wire it."
    echo

    if [[ -s "$unexpected_file" ]]; then
        echo "Newly unreachable (not in the baseline):"
        sed 's/^/  /' "$unexpected_file"
        echo
    fi

    if [[ -s "$stale_file" ]]; then
        echo "Now reachable, so the baseline is stale — delete these lines:"
        sed 's/^/  /' "$stale_file"
        echo
    fi

    if [[ -n "$count_errors" ]]; then
        echo "Recorded counts are out of date:"
        printf '%s' "$count_errors"
    fi
} >&2

exit 1
