#!/usr/bin/env python3
"""Enforce measured Rust line-coverage floors for every production crate."""
import argparse
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]


def check(report, floors, root=ROOT):
    groups = {}
    for dataset in report["data"]:
        for file in dataset["files"]:
            path = Path(file["filename"]).resolve().relative_to(root.resolve()).as_posix()
            group = "/".join(path.split("/")[:2]) if path.startswith("crates/") else path.split("/")[0]
            lines = file["summary"]["lines"]
            if not 0 <= lines["covered"] <= lines["count"]:
                raise ValueError(f"Invalid line counts for {path}")
            for name in (group, "workspace"):
                covered, total = groups.get(name, (0, 0))
                groups[name] = (covered + lines["covered"], total + lines["count"])
    failures = []
    for group in sorted(set(groups) | set(floors)):
        covered, total = groups.get(group, (0, 0))
        if group not in floors:
            failures.append(f"{group}: add an explicit coverage floor for the new production group")
        elif not total:
            failures.append(f"{group}: missing coverage data (never treated as 100%)")
        else:
            percent = 100 * covered / total
            floor = floors[group]
            if not isinstance(floor, (int, float)) or not 0 <= floor <= 100:
                raise ValueError(f"Invalid coverage floor for {group}: {floor}")
            print(f"{group}: {percent:.2f}% ({covered}/{total} lines), minimum {floor}%")
            if percent < floor:
                failures.append(f"{group}: {percent:.2f}% is below {floor}%")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", nargs="?", default="coverage/rust/coverage.json")
    args = parser.parse_args()
    try:
        failures = check(json.loads(Path(args.report).read_text()), json.loads((ROOT / "tests/coverage-baseline.json").read_text()))
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Invalid or missing coverage report: {error}", file=sys.stderr)
        return 1
    for failure in failures:
        print(failure, file=sys.stderr)
    return int(bool(failures))


if __name__ == "__main__":
    sys.exit(main())
