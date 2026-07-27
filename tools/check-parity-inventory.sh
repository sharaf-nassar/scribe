#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  tools/check-parity-inventory.sh --staged
  tools/check-parity-inventory.sh --working-tree
  tools/check-parity-inventory.sh --range <base> <head>

Re-derive the 016 launch gate's parity metric from
specs/016-gpui-client-rebuild/parity-inventory.md and fail when the document
disagrees with itself or with the source.

The gate metric is the reachable-row count. A row is reachable when its
"Reachable from" cell names a symbol, and unreachable when the cell is an
em-dash `— (unwired …)` / `— (missing …)` marker. This check recounts every
table from the marker cells and then verifies:

  * each section heading's declared row count
  * each section's `**Reachability:**` footer
  * the `Reachability roll-up` table, including its Total row
  * the user-facing sentence, its percentage, and the in-client figure
  * that the message tables enumerate exactly the protocol enums, and the
    keybinding table exactly the parsed `Bindings` actions
  * that every `ServerMessage` row the live reader does not handle is
    explicitly annotated as a settings-window row
  * that every requirement id in spec.md's register is carried by a row that
    actually exists, so the row set is derived from the requirement set rather
    than from whichever surface happened to be tabulated

Nothing here is hand-maintained, so the numbers cannot go stale while beads
land.
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

cleanup() {
    if [[ -n "$temp_dir" && -d "$temp_dir" ]]; then
        rm -rf "$temp_dir"
    fi
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

SCAN_ROOT="$scan_root" CONTEXT="$context" perl <<'PERL'
use strict;
use warnings;

my $root = $ENV{SCAN_ROOT} // die "SCAN_ROOT is required\n";
my $context = $ENV{CONTEXT} // 'the tree';
my $doc_rel = 'specs/016-gpui-client-rebuild/parity-inventory.md';

# The inventory's marker cells open with a literal em dash, so every file is
# decoded rather than read as bytes: a byte-mode read would leave `—` as three
# characters and silently count every unreachable row as reachable.
binmode STDOUT, ':encoding(UTF-8)';
binmode STDERR, ':encoding(UTF-8)';
$| = 1;

sub slurp {
    my ($path) = @_;
    open my $fh, '<:encoding(UTF-8)', $path or die "open $path: $!\n";
    local $/;
    my $text = <$fh>;
    close $fh;
    return $text;
}

# Strip character literals, string literals and line comments so brace counting
# is not thrown off by punctuation inside doc comments or format strings.
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
    die "parity: could not find fn $name\n" unless defined $start;
    my $open = index($text, '{', $start);
    die "parity: could not find the body of fn $name\n" if $open < 0;
    my $depth = 0;
    for my $i ($open .. length($text) - 1) {
        my $ch = substr($text, $i, 1);
        $depth++ if $ch eq '{';
        if ($ch eq '}') {
            $depth--;
            return substr($text, $open + 1, $i - $open - 1) if $depth == 0;
        }
    }
    die "parity: unbalanced body for fn $name\n";
}

# Variant names of `pub enum <name>`, in declaration order.
sub enum_variants {
    my ($text, $name) = @_;
    my ($body) = $text =~ /\bpub enum \Q$name\E\s*\{(.*?)\n\}/s;
    die "parity: could not find pub enum $name\n" unless defined $body;
    my @variants = $body =~ /^    ([A-Z][A-Za-z0-9]*)\s*(?:\{|\(|,)/gm;
    die "parity: no variants parsed from enum $name\n" unless @variants;
    return @variants;
}

my @errors;
sub fail { push @errors, $_[0]; }

# ── Parse the inventory ─────────────────────────────────────────────────
#
# Sections are keyed by the roll-up label they must appear under, so the
# document's own roll-up table can be checked against the tables it summarises.
my @sections = (
    { prefix => 'Client messages',               label => 'Client messages' },
    { prefix => 'Server messages',               label => 'Server messages' },
    { prefix => 'Input and keybinding checklist', label => 'Input and keybinding actions' },
    { prefix => 'Rendering and window checklist', label => 'Rendering and window' },
    { prefix => 'Spec behaviour requirements',   label => 'Spec behaviour requirements' },
    { prefix => 'Removed configuration keys',    label => 'Removed configuration keys' },
);

my $doc = slurp("$root/$doc_rel");
my @lines = split /\n/, $doc, -1;

my $section;          # roll-up label of the section being read
my %declared;         # label => row count declared in the heading, if any
my %rows;             # label => [ { name, cell } ]
my %footer;           # label => { reachable, total, unwired, missing }
my @rollup;           # rows of the roll-up table
my @coverage;         # rows of the spec-requirement coverage index
my $block_start = -1;

sub cells {
    my ($line) = @_;
    my $inner = $line;
    $inner =~ s/^\s*\|//;
    $inner =~ s/\|\s*$//;
    return map { s/^\s+|\s+$//gr } split /\s*\|\s*/, $inner, -1;
}

sub flush_block {
    my ($from, $to) = @_;
    return if $to - $from < 2;    # header + separator + at least one row
    return unless $lines[$from + 1] =~ /^\|[\s:|-]+\|$/;
    my @header = cells($lines[$from]);
    my @body = map { [ cells($lines[$_]) ] } ($from + 2 .. $to);

    if ($header[0] eq 'Table') {
        @rollup = @body;
        return;
    }

    # The coverage index maps spec.md's requirement register onto rows. It is
    # not a parity table, so it must never be counted — but it is the link that
    # makes the row set derived from the requirement set.
    if ($header[0] eq 'spec.md requirement') {
        @coverage = @body;
        return;
    }

    return unless defined $section;
    my ($reach_at) = grep { $header[$_] eq 'Reachable from' } 0 .. $#header;
    unless (defined $reach_at) {
        fail("$doc_rel: the table under '$section' has no 'Reachable from' column");
        return;
    }
    for my $row (@body) {
        my $name = $row->[0] // '';
        $name =~ s/^`|`$//g;
        push @{ $rows{$section} }, { name => $name, cell => ($row->[$reach_at] // '') };
    }
}

for my $i (0 .. $#lines) {
    my $line = $lines[$i];
    if ($line =~ /^\|/) {
        $block_start = $i if $block_start < 0;
        next;
    }
    if ($block_start >= 0) {
        flush_block($block_start, $i - 1);
        $block_start = -1;
    }
    if ($line =~ /^## (.+)$/) {
        my $heading = $1;
        $section = undef;
        for my $spec (@sections) {
            next unless index($heading, $spec->{prefix}) == 0;
            $section = $spec->{label};
            $declared{$section} = $1 if $heading =~ /\((\d+)\b/;
            last;
        }
        next;
    }
    if ($line =~ /^\*\*Reachability:\*\* (\d+) of (?:the )?(\d+) (?:rows|actions)\b/) {
        my ($reachable, $total) = ($1, $2);
        unless (defined $section) {
            fail("$doc_rel:@{[$i + 1]}: a Reachability footer sits outside a parity section");
            next;
        }
        my ($unwired, $missing) = $line =~ /(\d+) are unwired and (\d+) are missing/;
        $footer{$section} = {
            reachable => $reachable,
            total     => $total,
            unwired   => $unwired,
            missing   => $missing,
        };
    }
}
flush_block($block_start, $#lines) if $block_start >= 0;

# ── Recount every table from its marker cells ───────────────────────────
my %count;
my $grand_total = 0;
for my $spec (@sections) {
    my $label = $spec->{label};
    my $table = $rows{$label};
    unless ($table && @$table) {
        fail("$doc_rel: no parity table found under '$label'");
        next;
    }
    my %tally = (rows => scalar @$table, reachable => 0, unwired => 0, missing => 0, out => 0);
    for my $row (@$table) {
        my $cell = $row->{cell};
        $tally{out}++ if $cell =~ /out-of-client/;
        if ($cell =~ /^\x{2014}/) {
            if    ($cell =~ /\(unwired/) { $tally{unwired}++ }
            elsif ($cell =~ /\(missing/) { $tally{missing}++ }
            else {
                fail("$doc_rel: '$row->{name}' opens with an em dash but names no"
                    . " unwired/missing marker: $cell");
                $tally{unwired}++;
            }
        } else {
            $tally{reachable}++;
        }
    }
    $count{$label} = \%tally;
    $grand_total += $tally{rows};

    if (defined $declared{$label} && $declared{$label} != $tally{rows}) {
        fail("$doc_rel: the '$label' heading declares $declared{$label} rows,"
            . " the table has $tally{rows}");
    }

    my $foot = $footer{$label};
    unless ($foot) {
        fail("$doc_rel: '$label' has no '**Reachability:**' footer");
        next;
    }
    fail("$doc_rel: '$label' footer says $foot->{reachable} of $foot->{total} reachable;"
        . " the table has $tally{reachable} of $tally{rows}")
        if $foot->{reachable} != $tally{reachable} || $foot->{total} != $tally{rows};
    if (defined $foot->{unwired}) {
        fail("$doc_rel: '$label' footer says $foot->{unwired} unwired / $foot->{missing}"
            . " missing; the table has $tally{unwired} / $tally{missing}")
            if $foot->{unwired} != $tally{unwired} || $foot->{missing} != $tally{missing};
    }
}

# ── The roll-up must be the sum of the tables ───────────────────────────
my %rollup_seen;
my %totals = (rows => 0, reachable => 0, unwired => 0, missing => 0);
for my $row (@rollup) {
    my ($label, @numbers) = @$row;
    $label =~ s/\*\*//g;
    if ($label eq 'Total') {
        my @want = map { $totals{$_} } qw(rows reachable unwired missing);
        my @got = map { s/\*\*//gr } @numbers;
        fail("$doc_rel: the roll-up Total row reads @got; the tables sum to @want")
            if "@got" ne "@want";
        next;
    }
    my $tally = $count{$label};
    unless ($tally) {
        fail("$doc_rel: the roll-up names an unknown table '$label'");
        next;
    }
    $rollup_seen{$label} = 1;
    my @want = map { $tally->{$_} } qw(rows reachable unwired missing);
    fail("$doc_rel: the roll-up row for '$label' reads @numbers; the table is @want")
        if "@numbers" ne "@want";
    $totals{rows} += $tally->{rows};
    $totals{reachable} += $tally->{reachable};
    $totals{unwired} += $tally->{unwired};
    $totals{missing} += $tally->{missing};
}
for my $spec (@sections) {
    fail("$doc_rel: the roll-up omits the '$spec->{label}' table")
        unless $rollup_seen{ $spec->{label} };
}

# ── The user-facing sentence and the in-client figure ───────────────────
my $removed = $count{'Removed configuration keys'} // { rows => 0, reachable => 0, out => 0 };
my $user_rows = $grand_total - $removed->{rows};
my $user_reachable =
    ($totals{reachable} || 0) - $removed->{reachable};
my $user_unreachable = $user_rows - $user_reachable;
my $user_percent = $user_rows ? int(100 * $user_reachable / $user_rows + 0.5) : 0;
my $out_of_client = 0;
$out_of_client += $count{ $_->{label} }{out} for grep { $count{ $_->{label} } } @sections;
my $in_client = $user_reachable - $out_of_client;

# The roll-up prose is hard-wrapped, so match it with newlines folded away
# rather than forbidding a line break inside a sentence.
my $prose = $doc =~ s/\s+/ /gr;

if ($prose =~ /\*\*(\d+) rows, of which (\d+) are reachable \((\d+)%\)\*\* and (\d+) are not/) {
    fail("$doc_rel: the user-facing sentence reads $1 rows / $2 reachable ($3%) / $4 not;"
        . " the tables give $user_rows / $user_reachable / $user_percent% / $user_unreachable")
        if $1 != $user_rows
        || $2 != $user_reachable
        || $3 != $user_percent
        || $4 != $user_unreachable;
} else {
    fail("$doc_rel: no user-facing roll-up sentence of the form"
        . " '**N rows, of which M are reachable (P%)** and Q are not'");
}

if ($prose =~ /\*\*(\d+) of those (\d+)\*\* rows/) {
    fail("$doc_rel: the out-of-client sentence reads $1 of $2; the tables give"
        . " $out_of_client of $user_rows")
        if $1 != $out_of_client || $2 != $user_rows;
} else {
    fail("$doc_rel: no out-of-client sentence of the form '**N of those M** rows'");
}

if ($prose =~ /in-client figure is \*\*(\d+) of (\d+)\*\*/) {
    fail("$doc_rel: the in-client figure reads $1 of $2; the tables give"
        . " $in_client of $user_rows")
        if $1 != $in_client || $2 != $user_rows;
} else {
    fail("$doc_rel: no in-client figure of the form 'in-client figure is **N of M**'");
}

# ── The coverage index must span spec.md's requirement register ─────────
#
# This is the check that keeps the row set derived from the requirement set.
# Without it the inventory can be internally perfect and still omit a whole
# requirement, which is exactly how nine spec requirements went unscored until
# 2026-07-27: a requirement with no row is measured by no oracle.
my $spec_rel = 'specs/016-gpui-client-rebuild/spec.md';
my $spec = slurp("$root/$spec_rel");
my @register = $spec =~ /^\s*- \*\*(US\d+-\d+|PO-\d+)\*\*/gm;
if (!@register) {
    fail("$spec_rel: no requirement register ids found; every acceptance"
        . " criterion and porting obligation must be tagged '- **US<n>-<n>**'"
        . " or '- **PO-<n>**'");
}

my %register_seen;
for my $id (@register) {
    fail("$spec_rel: requirement id '$id' is declared twice") if $register_seen{$id}++;
}

# Anything a coverage cell may legitimately point at: a row in any counted
# table, or a table label written as `§Label`.
my %row_name = map { $_->{name} => 1 } map { @{ $rows{$_} // [] } } map { $_->{label} } @sections;
my %table_label = map { $_->{label} => 1 } @sections;

my %covered;
for my $row (@coverage) {
    my ($id, $carriers) = @$row;
    $id =~ s/^`|`$//g;
    $carriers //= '';
    unless ($register_seen{$id}) {
        fail("$doc_rel: the coverage index names '$id', which is not a"
            . " requirement id in $spec_rel");
        next;
    }
    fail("$doc_rel: the coverage index lists '$id' twice") if $covered{$id}++;

    # `not a parity row` is the escape hatch for tree, licensing and CI
    # requirements, which no reachable client symbol can carry.
    next if $carriers =~ /\bnot a parity row\b/;

    my @named = $carriers =~ /`([^`]+)`/g;
    my @labels = $carriers =~ /§([A-Za-z][A-Za-z ]*[A-Za-z])/g;
    my @unknown = grep { !$row_name{$_} } @named;
    my @bad_labels = grep { !$table_label{$_} } @labels;
    fail("$doc_rel: the coverage cell for '$id' names rows that no table"
        . " contains: @unknown")
        if @unknown;
    fail("$doc_rel: the coverage cell for '$id' names unknown tables:"
        . " @bad_labels")
        if @bad_labels;
    fail("$doc_rel: the coverage cell for '$id' names no carrying row, table"
        . " or 'not a parity row' reason")
        unless @named || @labels;
}

my @uncovered = grep { !$covered{$_} } @register;
fail("$doc_rel: $spec_rel declares requirements with no carrying row in the"
    . " coverage index: @uncovered")
    if @uncovered;

# ── Cross-check the tables against the source they enumerate ────────────
sub compare_sets {
    my ($label, $want, $got) = @_;
    my %want = map { $_ => 1 } @$want;
    my %got = map { $_ => 1 } @$got;
    my @absent = grep { !$got{$_} } @$want;
    my @extra = grep { !$want{$_} } @$got;
    fail("$doc_rel: the '$label' table is missing: @absent") if @absent;
    fail("$doc_rel: the '$label' table names unknown entries: @extra") if @extra;
}

my $protocol = strip_noise(slurp("$root/crates/scribe-common/src/protocol.rs"));
compare_sets(
    'Client messages',
    [ enum_variants($protocol, 'ClientMessage') ],
    [ map { $_->{name} } @{ $rows{'Client messages'} // [] } ],
);
my @server_messages = enum_variants($protocol, 'ServerMessage');
compare_sets(
    'Server messages',
    \@server_messages,
    [ map { $_->{name} } @{ $rows{'Server messages'} // [] } ],
);

my $input = slurp("$root/crates/scribe-client/src/input.rs");
my ($bindings) = $input =~ /\bpub struct Bindings\s*\{(.*?)\n\}/s;
die "parity: could not find pub struct Bindings\n" unless defined $bindings;
my @actions = $bindings =~ /^    pub (\w+): BindingSet,/gm;
die "parity: no actions parsed from Bindings\n" unless @actions;
compare_sets(
    'Input and keybinding actions',
    \@actions,
    [ map { $_->{name} } @{ $rows{'Input and keybinding actions'} // [] } ],
);

# A `ServerMessage` the terminal window's reader does not act on is only
# legitimately reachable through the settings window's synchronous
# request/reply helper, and the row has to say so.
my $main_code = strip_noise(slurp("$root/crates/scribe-client-gpui/src/main.rs"));
my $reader = fn_body($main_code, 'dispatch_server_message');
my %handled = map { $_ => 1 } ($reader =~ /ServerMessage::(\w+)/g);
for my $row (@{ $rows{'Server messages'} // [] }) {
    next if $handled{ $row->{name} };
    next if $row->{cell} =~ /^\x{2014}/;
    next if $row->{cell} =~ /settings-window row/;
    fail("$doc_rel: '$row->{name}' is not handled by dispatch_server_message and its"
        . " row does not mark it a settings-window row");
}

printf
    "parity inventory: %d rows, %d reachable, %d unwired, %d missing"
    . " (%d user-facing, %d reachable in-client, %d spec requirements"
    . " carried)\n",
    $grand_total, $totals{reachable}, $totals{unwired}, $totals{missing},
    $user_rows, $in_client, scalar keys %covered;

if (@errors) {
    print STDERR "The 016 parity inventory drifted from itself or the source in $context.\n\n";
    print STDERR "parity-inventory.md is the launch gate's metric, so every number in it\n";
    print STDERR "is derived, never typed. Fix the rows, footers, roll-up and prose the\n";
    print STDERR "errors below name:\n\n";
    print STDERR "  $_\n" for @errors;
    exit 1;
}
PERL
