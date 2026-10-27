"""Build an Apple Silicon test app and zip, without cargo-bundle or signing secrets.

Run from the repository root: python3 scripts/package-macos.py --offline
An existing output directory is never overwritten. Python 3.11+ is required.
"""
import argparse
import hashlib
import importlib.util
import json
import platform
import plistlib
import re
import shutil
import subprocess
import sys
from pathlib import Path

if sys.version_info < (3, 11):
    raise SystemExit("Python 3.11+ is required (for example, python3.12).")

ROOT = Path(__file__).resolve().parents[1]


def run(*args, **kwargs):
    return subprocess.run(args, cwd=ROOT, check=True, **kwargs)


def build(options):
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise SystemExit("Build this test package on an Apple Silicon Mac.")
    output = options.output.resolve()
    if output.exists():
        raise SystemExit(f"Output already exists; choose a new --output: {output}")
    profile = "debug" if options.debug else "release"
    if not options.skip_build:
        command = ["cargo", "build", "--workspace", "--locked"]
        if not options.debug:
            command.append("--release")
        if options.offline:
            command.append("--offline")
        run(*command)
    binary = ROOT / "target" / profile / "lan-mouse"
    app = output / "Sharemouse.app"
    contents = app / "Contents"
    resources = contents / "Resources"
    executable = contents / "MacOS/lan-mouse"
    executable.parent.mkdir(parents=True)
    resources.mkdir()
    shutil.copy2(binary, executable)
    svg = ROOT / "assets/icons/menubar-template.svg"
    shutil.copy2(ROOT / "app-icon.icns", resources / "icon.icns")
    run("rsvg-convert", "-w", "44", "-h", "44", str(svg),
        "-o", str(resources / "menubar-template.png"))
    info = dict(
        CFBundleIdentifier="de.feschber.LanMouse", CFBundleName="Sharemouse",
        CFBundleDisplayName="Sharemouse", CFBundleExecutable="lan-mouse",
        CFBundleIconFile="icon", CFBundlePackageType="APPL",
        CFBundleShortVersionString="0.11.0", CFBundleVersion="0.11.0",
        LSUIElement=True, NSAppSleepDisabled=True,
        NSInputMonitoringUsageDescription="Sharemouse captures keyboard and mouse input for your authorized computers.",
        NSAppleEventsUsageDescription="Sharemouse controls keyboard and mouse input on this computer.",
        NSLocalNetworkUsageDescription="Sharemouse connects to your authorized computers on your local network.",
    )
    with open(contents / "Info.plist", "wb") as file:
        plistlib.dump(info, file)
    run("bash", str(ROOT / "scripts/copy-macos-dylib.sh"), str(executable))
    # Use the largest deployment target actually required by any shipped Mach-O.
    minimum = (11, 0)
    binaries = [executable] + list((contents / "Frameworks").iterdir())
    for binary_file in binaries:
        load = subprocess.check_output(["otool", "-l", str(binary_file)], text=True)
        for version in re.findall(r"\bminos\s+([\d.]+)", load):
            minimum = max(minimum, tuple(map(int, version.split("."))))
        links = subprocess.check_output(["otool", "-L", str(binary_file)], text=True)
        if any(prefix in links for prefix in ["/opt/homebrew/", "/usr/local/Cellar/", "/usr/local/opt/"]):
            raise RuntimeError(f"Unbundled dependency in {binary_file.name}")
    info["LSMinimumSystemVersion"] = ".".join(map(str, minimum))
    spec = importlib.util.spec_from_file_location("package_common", ROOT / "scripts/package-common.py")
    common = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(common)
    manifest = common.provenance(resources / "Source", executable)
    # The final bundle seal changes the main executable's code signature.
    # Its final hash must live outside the signed bundle to avoid a cycle.
    manifest["pre_sign_binary_sha256"] = manifest.pop("binary_sha256")
    manifest.update(architecture="arm64", minimum_macos=info["LSMinimumSystemVersion"],
                    profile=profile, os=platform.mac_ver()[0])
    (resources / "Source/build.json").write_text(json.dumps(manifest, indent=2) + "\n")
    # Preserve exact Homebrew formula versions, licensing and source URLs.
    brew = subprocess.check_output(["brew", "info", "--json=v2", "--installed"], text=True)
    formulas = json.loads(brew)["formulae"]
    (resources / "Source/homebrew-dependencies.json").write_text(
        json.dumps(formulas, indent=2) + "\n")
    cellar_root = Path(subprocess.check_output(["brew", "--cellar"], text=True).strip())
    for formula in formulas:
        cellar = cellar_root / formula["name"]
        if not cellar.is_dir():
            continue
        for version_dir in cellar.iterdir():
            for file in version_dir.iterdir():
                if file.is_file() and file.name.upper().startswith(("LICENSE", "COPYING", "NOTICE", "COPYRIGHT")):
                    target = resources / "Source/native-licenses" / formula["name"] / file.name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(file, target)
    for name in ["安装与卸载.md", "三机测试清单.md"]:
        shutil.copy2(ROOT / "docs" / name, resources / name)
    shutil.copy2(ROOT / "scripts/uninstall-macos.command", resources / "uninstall-macos.command")
    with open(contents / "Info.plist", "wb") as file:
        plistlib.dump(info, file)
    run("codesign", "--force", "--sign", "-", str(app))
    run("codesign", "--verify", "--deep", "--strict", str(app))
    run("plutil", "-lint", str(contents / "Info.plist"))
    run("ditto", "-c", "-k", "--sequesterRsrc", "--keepParent",
        str(app), str(output / "Sharemouse-macos-arm64.zip"))
    manifest["binary_sha256"] = hashlib.sha256(executable.read_bytes()).hexdigest()
    manifest["zip_sha256"] = hashlib.sha256(
        (output / "Sharemouse-macos-arm64.zip").read_bytes()).hexdigest()
    (output / "Sharemouse-macos-arm64.build.json").write_text(
        json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Package: {output / 'Sharemouse-macos-arm64.zip'}")
    print(f"Dependency minimum: macOS {info['LSMinimumSystemVersion']}; clean-machine acceptance pending.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/packages/macos-arm64")
    parser.add_argument("--debug", action="store_true")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--offline", action="store_true")
    build(parser.parse_args())
