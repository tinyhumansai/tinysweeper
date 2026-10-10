#!/usr/bin/env python3
"""Build the no-merge Embed spike without changing tinysweeper dependencies."""
import json
import os
from pathlib import Path
import subprocess
import sys
import shutil
import tomllib

PIN = "a1075723249eb6965acca95f3ca3a694e056c9e8"
ROOT = Path(__file__).resolve().parents[2]
SOURCE = Path(__file__).resolve().parent


def main():
    checkout = Path(os.environ["OPENHUMAN_CHECKOUT"]).resolve()
    rev = subprocess.check_output(["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
    if rev != PIN:
        raise SystemExit(f"Expected OpenHuman {PIN}, got {rev}")
    dirty = subprocess.check_output(["git", "-C", str(checkout), "status", "--porcelain", "--untracked-files=no"], text=True)
    if dirty:
        raise SystemExit("OpenHuman checkout must have no tracked edits")
    modules = subprocess.check_output(["git", "-C", str(checkout), "submodule", "status", "--recursive"], text=True)
    if any(line.startswith(("-", "+", "U")) for line in modules.splitlines()):
        raise SystemExit("Initialise OpenHuman submodules at their recorded pins first")
    workspace = tomllib.loads((checkout / "Cargo.toml").read_text())
    build = ROOT / "target" / "embed-spike-package"
    build.mkdir(parents=True, exist_ok=True)
    manifest = ['[workspace]', '[package]', 'name = "tinysweeper-embed-spike"',
                'version = "0.0.0"', 'edition = "2024"', 'publish = false',
                '[[bin]]', 'name = "embed-spike"', f'path = {json.dumps(str(SOURCE / "main.rs"))}',
                '[dependencies]', 'anyhow = "1"', 'async-trait = "0.1"',
                'serde_json = "1"', 'toml = "1"', 'tempfile = "3"', 'wiremock = "0.6"',
                'tokio = { version = "1", features = ["full"] }',
                'tinysweeper = { git = "https://github.com/tinyhumansai/tinysweeper", rev = "2afdd90d1ced1ea8433d4f6f825ac148c6b783c7" }',
                f'openhuman-embed = {{ path = {json.dumps(str(checkout / "crates/openhuman-embed"))}, default-features = false }}',
                f'openhuman-core = {{ package = "openhuman", path = {json.dumps(str(checkout / "crates/openhuman-core"))}, default-features = false }}']
    # Cargo ignores a dependency workspace's patches. Mirror the pinned
    # workspace, without hard-coding consumer-specific absolute paths.
    for registry, packages in workspace.get("patch", {}).items():
        manifest.append(f'[patch.{json.dumps(registry)}]')
        for name, spec in packages.items():
            if name in {"tinyflows", "tinychannels", "tinyinference-decisions"}:
                continue  # Unused by this narrow feature set; avoid unstable unused-patch lock ordering.
            if set(spec) != {"path"}:
                raise SystemExit(f"Unsupported patch {name}; inspect the new pin")
            manifest.append(f'{name} = {{ path = {json.dumps(str(checkout / spec["path"]))} }}')
    manifest.extend(['[profile.dev.package."*"]', 'debug = false'])
    (build / "Cargo.toml").write_text("\n".join(manifest) + "\n")
    lock = SOURCE / "Cargo.lock"
    if lock.exists():
        shutil.copyfile(lock, build / "Cargo.lock")
    action = sys.argv[1:]
    if not action or action[0] not in ("test", "run", "check", "clippy", "fmt"):
        raise SystemExit("Usage: OPENHUMAN_CHECKOUT=... run.py test|run|check|clippy|fmt [cargo arguments]")
    if lock.exists() and action[0] != "fmt" and "--locked" not in action:
        action.insert(1, "--locked")
    # Keep all build output under this checkout; never override a host's
    # shared target configuration through CARGO_TARGET_DIR.
    raise SystemExit(subprocess.call(["cargo", "+1.96.1", action[0], "--manifest-path", str(build / "Cargo.toml"), *action[1:]], cwd=ROOT))


if __name__ == "__main__":
    main()
