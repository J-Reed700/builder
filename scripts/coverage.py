#!/usr/bin/env python3
"""Measure the entire Rust workspace, optionally including real UI journeys."""
import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def run(*command, env=None):
    print("\nRunning: " + " ".join(map(str, command)), flush=True)
    subprocess.run(command, cwd=ROOT, env=env, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--extended", action="store_true", help="Include PTY, retrieval and Chromium journeys in Rust coverage")
    args = parser.parse_args()
    if args.extended and os.name == "nt":
        parser.error("Extended coverage requires Linux or macOS for graceful instrumented process shutdown; use the standard coverage run on Windows")
    run("cargo", "llvm-cov", "--version")
    output = ROOT / "coverage/rust"
    output.mkdir(parents=True, exist_ok=True)
    run("cargo", "llvm-cov", "clean", "--workspace")
    run("cargo", "llvm-cov", "--workspace", "--locked", "--no-report")
    if args.extended:
        metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT))
        target = Path(os.environ.get("CARGO_LLVM_COV_TARGET_DIR", Path(metadata["target_directory"]) / "llvm-cov-target")).resolve()
        env = {**os.environ, "CARGO_TARGET_DIR": str(target)}
        # Parse assignments emitted by the installed coverage tool as data;
        # never evaluate shell code or hardcode toolchain instrumentation flags.
        assignments = subprocess.check_output(["cargo", "llvm-cov", "show-env"], cwd=ROOT, env=env, text=True)
        for line in assignments.splitlines():
            key, separator, value = line.partition("=")
            if not separator or not key.replace("_", "").isalnum():
                raise ValueError(f"Unexpected coverage environment assignment: {line}")
            values = shlex.split(value)
            if len(values) != 1:
                raise ValueError(f"Invalid coverage environment value for {key}")
            env[key] = values[0]
        run("cargo", "build", "--locked", "--bin", "builder", env=env)
        binary = str(target / "debug" / ("builder.exe" if os.name == "nt" else "builder"))
        if os.name != "nt":
            from test import TERMINAL_SUITES
            for suite in TERMINAL_SUITES:
                run(sys.executable, "tests/" + suite, binary, env=env)
        else:
            print("Unix PTY coverage is unavailable on Windows.")
        run("cargo", "run", "--locked", "--example", "retrieval_eval", "--", "eval/retrieval/cases.json", env=env)
        env["BUILDER_TEST_BINARY"] = binary
        run("npm.cmd" if os.name == "nt" else "npm", "run", "test:browser", "--", "--project=chromium", env=env)
    extra_gate = ["--fail-under-lines", "83"] if args.extended else []
    run("cargo", "llvm-cov", "report", "--workspace", "--json", "--output-path", str(output / "coverage.json"), *extra_gate)
    if args.extended:
        report = json.loads((output / "coverage.json").read_text())
        required = ["src/commands/remote.rs"]
        if os.name != "nt":
            required.append("src/input/screen.rs")
        for suffix in required:
            matches = [file for data in report["data"] for file in data["files"] if Path(file["filename"]).as_posix().endswith(suffix)]
            if not matches or not all(file["summary"]["lines"]["covered"] > 0 for file in matches):
                raise RuntimeError(f"Extended coverage did not record {suffix}; check instrumented binary and graceful shutdown")
    run("cargo", "llvm-cov", "report", "--workspace", "--lcov", "--output-path", str(output / "lcov.info"))
    run("cargo", "llvm-cov", "report", "--workspace", "--html", "--output-dir", str(output))
    run(sys.executable, "tests/test_coverage_gate.py")
    run(sys.executable, "scripts/check_coverage.py")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode)
