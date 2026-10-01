#!/usr/bin/env python3
"""Build a deterministic native Linux x86_64 or macOS arm64 candidate; never install or publish."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
# Native (system, machine) -> Rust host triple. Cross-compilation is not supported.
TARGETS = {("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
           ("Darwin", "arm64"): "aarch64-apple-darwin"}
EVIDENCE = ("docs/v0.4-cli-control-compatibility.md", "docs/v0.4-recovery-verification.md")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def canonical(value):
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()


def sources():
    paths = {ROOT / name for name in ("Cargo.toml", "Cargo.lock", "build.rs", "scripts/package_candidate.py", "scripts/smoke_candidate.py", ".github/workflows/ci.yml")}
    paths.update(ROOT / name for name in EVIDENCE)
    paths.update(ROOT.glob("tests/**/*.rs"))
    paths.update(ROOT.glob("tests/corpus/**/*.json"))
    paths.add(ROOT / "tests/test_candidate_packaging.py")
    paths.update(ROOT.glob("src/**/*.rs"))
    paths.update(ROOT.glob("src/exec/*.py"))
    # Workspace crates (fridica-core) are part of the same build.
    paths.update(ROOT.glob("crates/*/Cargo.toml"))
    paths.update(ROOT.glob("crates/*/src/**/*"))
    paths.update(ROOT.glob("crates/*/tests/**/*.rs"))
    paths.update(ROOT.glob("src/store/migrations/*.sql"))
    paths.update(ROOT.glob("assets/**/*"))
    paths.update(ROOT.glob("packaging/*.py"))
    paths.update(ROOT.glob("packaging/*.md"))
    paths.update(ROOT / name for name in ("src/config/template.toml",))
    return {str(path.relative_to(ROOT)): digest(path.read_bytes()) for path in sorted(paths) if path.is_file()}


def archive_bytes(files):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, (data, mode) in sorted(files.items()):
            entry = tarfile.TarInfo(name)
            entry.size, entry.mode, entry.mtime = len(data), mode, 0
            entry.uid = entry.gid = 0
            archive.addfile(entry, io.BytesIO(data))
    return gzip.compress(raw.getvalue(), mtime=0)


def native_target(system, machine, toolchain):
    """Return the supported native host triple, or refuse before building."""
    expected = TARGETS.get((system, machine))
    if expected is None:
        raise SystemExit("This candidate recipe supports native Linux x86_64 and macOS arm64 only")
    if re.search(r"^host: (.+)$", toolchain, re.M)[1] != expected:
        raise SystemExit(f"native {expected} toolchain required")
    return expected


def linkage(binaries):
    """Minimum platform and shared libraries of the built binaries."""
    paths = [binaries / "fridica", binaries / "fridica-overseer"]
    if platform.system() == "Darwin":
        loads = subprocess.check_output(["otool", "-l", *paths], text=True)
        minimum = max(set(re.findall(r"^\s*minos (\S+)$", loads, re.M)),
                      key=lambda v: tuple(map(int, v.split('.'))))
        linked = subprocess.check_output(["otool", "-L", *paths], text=True)
        libraries = {line.split(" (")[0].strip() for line in linked.splitlines() if line.startswith("\t")}
        return {"macos_minimum": minimum, "shared_libraries": sorted(libraries),
                "python": "/usr/bin/python3 for helpers; Python 3.11+ (e.g. Homebrew) for isolation "
                          "bootstrap and packaged operator tools",
                "confinement": "none locally: local worker isolation requires Linux; backend CLI sandbox applies"}
    linked = subprocess.check_output(["readelf", "--version-info", *paths], text=True)
    glibc = max(set(re.findall(r"GLIBC_(\d+\.\d+)", linked)), key=lambda v: tuple(map(int, v.split('.'))))
    needed = subprocess.check_output(["readelf", "-d", *paths], text=True)
    return {"glibc_minimum": glibc, "shared_libraries": sorted(set(re.findall(r'Shared library: \[(.+?)\]', needed))),
            "python": "/usr/bin/python3 (3.11+ for packaged operator tools)"}


def build(output, target_dir):
    identity = sources()
    source_id = digest(canonical(identity))
    toolchain = subprocess.check_output(["rustc", "-vV"], text=True)
    target = native_target(platform.system(), platform.machine(), toolchain)
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    environment = dict(os.environ)
    # Pin build-affecting flags; source paths and registry paths cannot enter binaries.
    for key in list(environment):
        if key.startswith(("CARGO_PROFILE_", "CARGO_TARGET_", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")):
            del environment[key]
    cargo_home = Path(environment.get("CARGO_HOME", Path.home() / ".cargo")).resolve()
    environment.update(FRIDICA_BUILD_ID=source_id, FRIDICA_BUILD_TARGET=target, SOURCE_DATE_EPOCH="0",
                       CARGO_INCREMENTAL="0", CARGO_TARGET_DIR=str(target_dir.resolve()),
                       CARGO_ENCODED_RUSTFLAGS="\x1f".join([
                           f"--remap-path-prefix={ROOT}=/fridica",
                           f"--remap-path-prefix={cargo_home}=/cargo", "-C", "debuginfo=0"]))
    subprocess.run(["cargo", "build", "--release", "--locked", "--offline", "--bins", "--target", target],
                   cwd=ROOT, env=environment, check=True)
    files = {}
    binaries = target_dir / target / "release"
    for name in ("fridica", "fridica-overseer"):
        files[f"bin/{name}-candidate"] = ((binaries / name).read_bytes(), 0o755)
    with tempfile.TemporaryDirectory() as tmp:
        assets = Path(tmp) / "assets"
        subprocess.run([binaries / "fridica", "assets", "--export", assets], check=True, stdout=subprocess.DEVNULL)
        for path in sorted(assets.rglob("*")):
            if path.is_file():
                files["share/assets/" + str(path.relative_to(assets))] = (path.read_bytes(), 0o644)
    for path in sorted((ROOT / "packaging").iterdir()):
        if path.suffix in (".py", ".md"):
            files["share/" + path.name] = (path.read_bytes(), 0o644)
    for name in EVIDENCE:
        files["share/evidence/" + Path(name).name] = ((ROOT / name).read_bytes(), 0o644)
    files["share/Cargo.lock"] = ((ROOT / "Cargo.lock").read_bytes(), 0o644)
    if sources() != identity:
        raise RuntimeError("source changed during build; rebuild the candidate")
    build_info = json.loads(subprocess.check_output([binaries / "fridica", "build-info"]))
    assert build_info == {"version": version, "source_id": source_id, "target": target}
    manifest = {"format": 1, "build": build_info, "rustc": toolchain,
                "platform": dict(linkage(binaries),
                                 scope="native development candidate; no cross-platform wheel certification"),
                "sources": identity,
                "files": {name: {"sha256": digest(data), "mode": mode, "size": len(data)}
                          for name, (data, mode) in files.items()}}
    files["build-manifest.json"] = (canonical(manifest), 0o644)
    data = archive_bytes(files)
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"fridica-candidate-{version}-{source_id[:12]}-{target}.tar.gz"
    archive.write_bytes(data)
    archive.with_suffix(archive.suffix + ".sha256").write_text(f"{digest(data)}  {archive.name}\n")
    # Standalone installer needs no package/source imports; trust its digest alongside archive.
    shutil.copyfile(ROOT / "packaging/install.py", output / "install-candidate.py")
    print(json.dumps({"archive": str(archive.resolve()), "sha256": digest(data), "build": build_info}, indent=2))
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "dist/candidate")
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target/candidate")
    parser.add_argument("--verify-reproducible", action="store_true", help="compare a second build in an empty target directory")
    parser.add_argument("--smoke", action="store_true", help="run isolated installed rehearsal (needs local sockets and /var/tmp)")
    args = parser.parse_args()
    archive = build(args.output.resolve(), args.target_dir.resolve())
    expected = digest(archive.read_bytes())
    if args.verify_reproducible:
        with tempfile.TemporaryDirectory(prefix="candidate-rebuild-", dir=ROOT / "target") as tmp:
            second = build(Path(tmp) / "output", Path(tmp) / "build")
            reproduced = digest(second.read_bytes())
            if reproduced != expected:
                raise RuntimeError(f"native rebuild differs: {expected} != {reproduced}")
        (args.output / "reproducibility.json").write_bytes(canonical({
            "archive": archive.name, "sha256": expected, "independent_target_builds_identical": True,
            "scope": "same source, native toolchain and build host; separate clean target directory"}))
    if args.smoke:
        subprocess.run([sys.executable, ROOT / "scripts/smoke_candidate.py", archive, "--sha256", expected,
                        "--report", args.output.resolve() / "candidate-smoke.json"], check=True)


if __name__ == "__main__":
    main()
