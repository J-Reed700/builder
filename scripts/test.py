#!/usr/bin/env python3
"""Run the deterministic test layers; no model credentials or downloads are used."""
import argparse
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
TERMINAL_SUITES = [
    "terminal_smoke.py", "terminal_interrupt.py", "terminal_compact.py",
    "terminal_pipeline.py", "terminal_memory.py",
]


def run(*command):
    print("\nRunning: " + " ".join(map(str, command)), flush=True)
    subprocess.run(command, cwd=ROOT, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-browsers", action="store_true", help="Only run Rust, Jest, retrieval and terminal layers")
    parser.add_argument("--docker", action="store_true", help="Also build/run the Docker gateway scenario")
    args = parser.parse_args()
    npm = "npm.cmd" if os.name == "nt" else "npm"
    run("cargo", "fmt", "--all", "--check")
    run("cargo", "clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings")
    run("cargo", "test", "--workspace", "--locked")
    run(npm, "test")
    run(sys.executable, "tests/test_coverage_gate.py")
    run("node", "tests/remote_web.cjs")
    run("cargo", "build", "--locked", "--bin", "builder")
    run("cargo", "run", "--locked", "--example", "retrieval_eval", "--", "eval/retrieval/cases.json")
    run("cargo", "test", "--locked", "--example", "retrieval_eval")
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).resolve()
    suffix = ".exe" if os.name == "nt" else ""
    run(sys.executable, "tests/retrieval_eval_cli.py", str(target / "debug/examples" / ("retrieval_eval" + suffix)))
    binary = str(target / "debug" / ("builder" + suffix))
    if os.name == "nt":
        print("Unix PTY tests are unavailable on Windows; CI runs them on Linux and macOS.")
    else:
        for suite in TERMINAL_SUITES:
            run(sys.executable, "tests/" + suite, binary)
    if not args.skip_browsers:
        run(npm, "run", "test:browser")
    if args.docker:
        run(sys.executable, "tests/remote_container.py", binary)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode)
