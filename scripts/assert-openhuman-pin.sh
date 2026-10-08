#!/bin/sh
# Assert that the three places naming the OpenHuman commit agree, and that this
# crate's `[patch]` tables still mirror OpenHuman's own.
#
# - `Cargo.toml` pins the git `openhuman-embed` dependency at a `rev`;
# - the `vendor/openhuman` submodule records a commit, which the `[patch]` in
#   `Cargo.toml` actually builds against;
# - `Cargo.lock` records the source it resolved the git dependency from.
#
# The patch makes every build here use the submodule, so a rev that drifted from
# it would be silently ignored locally and only bite whoever builds without the
# patch. And Cargo ignores a dependency's own `[patch]` sections, so if OpenHuman
# adds or drops a patched crate and ours does not follow, the core resolves that
# crate from a different source than OpenHuman tests against. Neither failure
# reports itself, so this script does.
set -eu
cd "$(dirname "$0")/.."

fail() {
  echo "assert-openhuman-pin: $*" >&2
  exit 1
}

rev=$(sed -n 's/^openhuman-embed = { git = "https:\/\/github.com\/tinyhumansai\/openhuman", rev = "\([0-9a-f]*\)".*/\1/p' Cargo.toml)
[ -n "$rev" ] || fail "could not read the openhuman-embed rev from Cargo.toml"

sub=$(git ls-tree HEAD vendor/openhuman | awk '{print $3}')
[ -n "$sub" ] || fail "vendor/openhuman is not a submodule at HEAD"
[ "$rev" = "$sub" ] || fail "Cargo.toml pins openhuman-embed at $rev but vendor/openhuman records $sub"

grep -q "git+https://github.com/tinyhumansai/openhuman?rev=$rev#$rev" Cargo.lock \
  || fail "Cargo.lock does not resolve openhuman-embed from rev $rev; run cargo update -p openhuman-embed"

# Every crate OpenHuman patches from the tinytools and tinyinference sources
# must be patched here too (crates-io entries for features this crate does not
# enable are deliberately left out; see the comment above the tables).
patched_names() {
  awk -v table="$2" '
    /^\[patch/ { in_table = index($0, table) > 0; next }
    /^\[/ { in_table = 0 }
    in_table && /^[a-z0-9_-]+ = / { print $1 }
  ' "$1" | sort
}
for table in 'tinyhumansai/tinytools' 'tinyhumansai/tinyinference'; do
  theirs=$(patched_names vendor/openhuman/Cargo.toml "$table")
  ours=$(patched_names Cargo.toml "$table")
  [ "$theirs" = "$ours" ] || fail "the [patch] table for $table differs from vendor/openhuman/Cargo.toml:
ours:   $(echo $ours)
theirs: $(echo $theirs)"
done

echo "assert-openhuman-pin: openhuman-embed $rev"
