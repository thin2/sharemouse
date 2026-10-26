"""Shared test-package provenance. Does not copy runtime configuration or keys."""
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile
import sys
if sys.version_info < (3, 11):
    raise SystemExit("Python 3.11+ is required to inventory Cargo.lock.")
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def command(*args):
    # Windows 上使用完整路径调用 git
    if args[0] == "git" and sys.platform == "win32":
        # 尝试多个常见 Git 安装位置
        git_paths = [
            r"D:\Git\cmd\git.exe",
            r"D:\Program Files\Git\cmd\git.exe",
            r"C:\Program Files\Git\cmd\git.exe",
            r"C:\Program Files (x86)\Git\cmd\git.exe",
        ]
        for git_exe in git_paths:
            if Path(git_exe).exists():
                args = (git_exe,) + args[1:]
                break
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def provenance(destination, executable):
    destination = Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    # Only version-controlled source, including current modifications.
    paths = command("git", "ls-files", "-z").split("\0")
    if command("git", "ls-files", "--others", "--exclude-standard"):
        raise RuntimeError("Stage intended source files before packaging; untracked files are not archived.")
    with tarfile.open(destination / "sharemouse-source.tar.gz", "w:gz") as archive:
        for name in sorted(set(filter(None, paths))):
            path = ROOT / name
            if path.is_file() and not path.is_symlink():
                archive.add(path, arcname="sharemouse/" + name, recursive=False)
    packages = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))["package"]
    crates = []
    licenses = destination / "crate-licenses"
    cache = Path.home() / ".cargo/registry/src"
    for package in packages:
        if not package.get("source", "").startswith("registry+"):
            continue
        item = {key: package[key] for key in ("name", "version", "checksum") if key in package}
        item["source"] = f"https://crates.io/api/v1/crates/{package['name']}/{package['version']}/download"
        roots = list(cache.glob(f"*/{package['name']}-{package['version']}"))
        if roots:
            metadata = tomllib.loads((roots[0] / "Cargo.toml").read_text(encoding="utf-8"))["package"]
            item["license"] = metadata.get("license", "See crate source")
            for file in roots[0].iterdir():
                if file.is_file() and file.name.upper().startswith(("LICENSE", "COPYING", "NOTICE", "COPYRIGHT")):
                    target = licenses / f"{package['name']}-{package['version']}" / file.name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(file.read_bytes())
        else:
            item["license"] = "See source archive (not cached on build host)"
        crates.append(item)
    (destination / "rust-dependencies.json").write_text(json.dumps(crates, indent=2) + "\n")
    manifest = dict(
        commit=command("git", "rev-parse", "HEAD"),
        modified=bool(command("git", "status", "--porcelain")),
        binary_sha256=hashlib.sha256(Path(executable).read_bytes()).hexdigest(),
        rust=command("rustc", "--version"),
        protocol=7,
        signing="Local test package; no distribution signature or notarization",
    )
    (destination / "build.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (destination / "LICENSE").write_bytes((ROOT / "LICENSE").read_bytes())
    return manifest


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("destination", type=Path)
    parser.add_argument("executable", type=Path)
    arguments = parser.parse_args()
    provenance(arguments.destination, arguments.executable)
