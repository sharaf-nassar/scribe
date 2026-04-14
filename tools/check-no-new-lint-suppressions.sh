#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  tools/check-no-new-lint-suppressions.sh --staged
  tools/check-no-new-lint-suppressions.sh --working-tree
  tools/check-no-new-lint-suppressions.sh --range <base> <head>

Reject Rust lint suppressions outside the approved baseline inventory.
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
found_file=""
expected_file=""
unexpected_file=""
missing_file=""
found_keys_file=""

cleanup() {
    if [[ -n "$temp_dir" && -d "$temp_dir" ]]; then
        rm -rf "$temp_dir"
    fi
    rm -f "$found_file" "$expected_file" "$unexpected_file" "$missing_file" "$found_keys_file"
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

allowlist_file="$scan_root/tools/lint-suppressions-allowlist.txt"
if [[ ! -f "$allowlist_file" ]]; then
    echo "error: missing lint suppression allowlist at $allowlist_file" >&2
    exit 2
fi

found_file="$(mktemp)"
expected_file="$(mktemp)"
unexpected_file="$(mktemp)"
missing_file="$(mktemp)"
found_keys_file="$(mktemp)"

SCAN_ROOT="$scan_root" perl <<'PERL' >"$found_file"
use strict;
use warnings;
use File::Find;

my $root = $ENV{SCAN_ROOT} // die "SCAN_ROOT is required\n";
my @rows;

sub bracket_delta {
    my ($text) = @_;
    my $open = () = $text =~ /\[/g;
    my $close = () = $text =~ /\]/g;
    return $open - $close;
}

find(
    {
        no_chdir => 1,
        wanted => sub {
            if (-d $File::Find::name
                && $File::Find::name =~ m{(?:^|/)(?:\.git|target|node_modules|third_party)(?:/|$)}) {
                $File::Find::prune = 1;
                return;
            }
            return unless -f $_ && $_ =~ /\.rs\z/;
            my $path = $File::Find::name;
            open my $fh, '<', $path or die "open $path: $!\n";
            my @lines = <$fh>;

            for (my $i = 0; $i <= $#lines; $i++) {
                next unless $lines[$i] =~ /^\s*#\s*!?\[/;

                my $attr = $lines[$i];
                my $depth = bracket_delta($lines[$i]);
                my $j = $i;

                while ($depth > 0 && $j < $#lines) {
                    $j++;
                    $attr .= $lines[$j];
                    $depth += bracket_delta($lines[$j]);
                }

                my $compact = $attr;
                $compact =~ s/\s+//g;

                my $canonical;
                if ($compact =~ /^(#\!?\[)(allow|expect)\((.*)\)\]$/s) {
                    my ($prefix, $kind, $args) = ($1, $2, $3);
                    $args =~ s/,reason="(?:[^"\\]|\\.)*"//g;
                    $args =~ s/^,//;
                    $args =~ s/,$//;
                    $canonical = $prefix . $kind . q{(} . $args . q{)]};
                } elsif ($compact =~ /^#\!?\[cfg_attr\(.*\b(?:allow|expect)\(.*\).*\)\]$/s) {
                    $canonical = $compact;
                } else {
                    next;
                }

                my $display = $attr;
                $display =~ s/\s+/ /g;
                $display =~ s/^\s+//;
                $display =~ s/\s+$//;

                my $relative = $path;
                $relative =~ s/^\Q$root\E\/?//;
                my $key = $relative . q{|} . $canonical;
                my $line_number = $i + 1;
                push @rows, join("\t", $key, "$relative:$line_number", $display);
                $i = $j;
            }
        },
    },
    $root,
);

print "$_\n" for sort @rows;
PERL

sort "$allowlist_file" >"$expected_file"
cut -f1 "$found_file" | sort >"$found_keys_file"

comm -23 "$found_keys_file" "$expected_file" | uniq >"$unexpected_file" || true
comm -13 "$found_keys_file" "$expected_file" | uniq >"$missing_file" || true

if [[ ! -s "$unexpected_file" && ! -s "$missing_file" ]]; then
    exit 0
fi

{
    echo "Rust lint suppressions outside the approved baseline were found in $context."
    echo
    echo "Fix the underlying lint instead of adding a suppression attribute."
    echo "If a suppression is truly unavoidable, update tools/lint-suppressions-allowlist.txt deliberately."
    echo

    if [[ -s "$unexpected_file" ]]; then
        echo "Unexpected suppressions:"
        while IFS= read -r key; do
            awk -F '\t' -v target="$key" '$1 == target { printf "  %s %s\n", $2, $3 }' "$found_file"
        done <"$unexpected_file"
        echo
    fi

    if [[ -s "$missing_file" ]]; then
        echo "Missing baseline suppressions:"
        sed 's/^/  /' "$missing_file"
    fi
} >&2

exit 1
