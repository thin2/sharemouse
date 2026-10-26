"""Write a small diagnostic report without clipboard, messages, keys or file names."""
import argparse
import hashlib
import json
import platform
import sys
if sys.version_info < (3, 11):
    raise SystemExit("Python 3.11+ is required.")
import tomllib
from pathlib import Path


def collect(binary, config=None):
    report = dict(os=platform.platform(), architecture=platform.machine(),
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    manifests = [binary.parent.parent / "Resources/Source/build.json",
                 binary.parent / "Source/build.json"]
    for manifest in manifests:
        if manifest.is_file():
            report["build"] = json.loads(manifest.read_text(encoding="utf-8"))
            break
    if config is not None:
        data = tomllib.loads(config.read_text(encoding="utf-8"))
        report["configuration"] = {key: data[key] for key in
            ["port", "data_port", "capture_backend", "emulation_backend"] if key in data}
        report["configuration"]["client_count"] = len(data.get("clients", []))
        report["configuration"]["authorized_count"] = len(data.get("authorized_fingerprints", {}))
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--config", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    report = collect(args.binary, args.config)
    with args.output.open("x", encoding="utf-8") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    print(f"Saved diagnostic metadata: {args.output}")
