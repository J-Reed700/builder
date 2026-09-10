"""Exercise the evaluation executable's failure contract, using temporary manifests."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve()
if sys.platform == "win32" and binary.suffix != ".exe":
    binary = binary.with_suffix(".exe")
corpus = Path(__file__).resolve().parents[1] / "eval" / "retrieval"
original = json.loads((corpus / "cases.json").read_text())
for case in original:
    case["workspace"] = str(corpus / case["workspace"])


def run(cases):
    with tempfile.TemporaryDirectory() as directory:
        manifest = Path(directory) / "cases.json"
        manifest.write_text(json.dumps(cases))
        # Keep this a deterministic lexical test even on machines with models.
        import os
        environment = os.environ.copy()
        environment.pop("BUILDER_LOCAL_MODEL_DIR", None)
        return subprocess.run(
            [str(binary), str(manifest)], capture_output=True, text=True,
            timeout=60, check=False, env=environment,
        )


bad_expectation = json.loads(json.dumps(original))
bad_expectation[-1]["query"] = "retry_transport"
result = run(bad_expectation)
assert result.returncode != 0, "A quality failure returned success"
reports = [json.loads(line) for line in result.stdout.splitlines()]
assert len(reports) == len(original) * 2, "Quality failure discarded later reports"
assert sum(not report["assessment"]["passed"] for report in reports) == 2

stale = json.loads(json.dumps(original))
stale[0]["source_hashes"]["distractors.rs"] = "0" * 64
result = run(stale)
assert result.returncode != 0 and "Stale evaluation source" in result.stderr

out_of_file = json.loads(json.dumps(original))
out_of_file[0]["relevant"][0]["lines"] = [100000, 100001]
result = run(out_of_file)
assert result.returncode != 0 and "Label extends past source" in result.stderr
print("Evaluation CLI: quality failures, retained reports, stale distractors, and invalid labels passed")
