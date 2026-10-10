#!/bin/sh
# Assert that the two places naming the OpenHuman commit agree, and that this
# crate's `[patch]` tables still mirror OpenHuman's own.
#
# - `Cargo.toml` pins the OpenHuman source in package metadata;
# - the `vendor/openhuman` submodule records a commit, which the `[patch]` in
#   `Cargo.toml` actually builds against. (`Cargo.lock` cannot carry the rev:
#   a path package is locked with no source.)
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

rev=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["metadata"]["openhuman"]["rev"])')
[ -n "$rev" ] || fail "could not read the openhuman-embed rev from Cargo.toml"

sub=$(git ls-tree HEAD vendor/openhuman | awk '{print $3}')
[ -n "$sub" ] || fail "vendor/openhuman is not a submodule at HEAD"
[ "$rev" = "$sub" ] || fail "Cargo.toml pins openhuman-embed at $rev but vendor/openhuman records $sub"

# Compare every upstream patch, including optional feature dependencies.
python3 - <<'CHECK'
import pathlib
import tomllib
root = tomllib.loads(pathlib.Path("Cargo.toml").read_text())
upstream = tomllib.loads(pathlib.Path("vendor/openhuman/Cargo.toml").read_text())
for source, packages in upstream.get("patch", {}).items():
    ours = root.get("patch", {}).get(source, {})
    expected = {name: {"path": "vendor/openhuman/" + spec["path"]}
                for name, spec in packages.items()}
    if ours != expected:
        raise SystemExit(f"assert-openhuman-pin: patch table differs: {source}")
CHECK

echo "assert-openhuman-pin: openhuman-embed $rev"
