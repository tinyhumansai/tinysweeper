#!/usr/bin/env python3
"""Apply `presets/labels.toml` to a repository's labels.

Idempotent: run it as often as you like. It creates what is missing and
corrects the colour and description of what is not, and it reports every change
rather than applying them silently.

It does **not** delete labels absent from the file unless `--prune` is given,
and even then it only offers to remove the ones this vocabulary owns. A
maintainer's own labels are not this script's business, and a label deleted by
a tool takes its assignments with it — that is not recoverable from the API.

    scripts/sync-labels.py --repo owner/name
    scripts/sync-labels.py --repo owner/name --dry-run
    scripts/sync-labels.py --repo owner/name --prune
"""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys
import tomllib

# Only these prefixes are ever considered for pruning. Everything else is
# somebody else's, including GitHub's own defaults.
OWNED_PREFIXES = ("priority: ", "severity: ", "tinysweeper:")

HERE = pathlib.Path(__file__).resolve().parent.parent
LABELS = HERE / "presets" / "labels.toml"


def gh(args: list[str], **kw) -> subprocess.CompletedProcess:
    return subprocess.run(["gh", *args], capture_output=True, text=True, **kw)


def existing(repo: str) -> dict[str, dict]:
    out = gh(["label", "list", "--repo", repo, "--limit", "200",
              "--json", "name,color,description"])
    if out.returncode != 0:
        sys.exit(f"could not list labels: {out.stderr.strip()}")
    return {l["name"]: l for l in json.loads(out.stdout or "[]")}


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--repo", required=True, help="owner/name")
    p.add_argument("--dry-run", action="store_true", help="say what would change")
    p.add_argument("--prune", action="store_true",
                   help="also delete owned labels no longer in the vocabulary")
    args = p.parse_args()

    wanted = tomllib.loads(LABELS.read_text())["labels"]
    have = existing(args.repo)
    changed = 0

    for label in wanted:
        name, color = label["name"], label["color"]
        description = label.get("description", "")
        current = have.get(name)

        if current is None:
            print(f"  create  {name}")
            changed += 1
            if not args.dry_run:
                r = gh(["label", "create", name, "--repo", args.repo,
                        "--color", color, "--description", description])
                if r.returncode != 0:
                    print(f"          failed: {r.stderr.strip()}", file=sys.stderr)
            continue

        drift = []
        if current["color"].lower() != color.lower():
            drift.append(f"color {current['color']} -> {color}")
        if (current.get("description") or "") != description:
            drift.append("description")
        if not drift:
            continue

        print(f"  update  {name}  ({', '.join(drift)})")
        changed += 1
        if not args.dry_run:
            r = gh(["label", "edit", name, "--repo", args.repo,
                    "--color", color, "--description", description])
            if r.returncode != 0:
                print(f"          failed: {r.stderr.strip()}", file=sys.stderr)

    if args.prune:
        names = {l["name"] for l in wanted}
        for name in sorted(have):
            if name in names or not name.startswith(OWNED_PREFIXES):
                continue
            # Deleting a label removes it from every issue carrying it, and the
            # API will not give those assignments back. Loud, and opt-in twice.
            print(f"  DELETE  {name}  (owned, no longer in the vocabulary)")
            changed += 1
            if not args.dry_run:
                r = gh(["label", "delete", name, "--repo", args.repo, "--yes"])
                if r.returncode != 0:
                    print(f"          failed: {r.stderr.strip()}", file=sys.stderr)

    if changed == 0:
        print("labels already match the vocabulary")
    elif args.dry_run:
        print(f"\n{changed} change(s) — nothing applied, this was a dry run")
    else:
        print(f"\n{changed} change(s) applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
