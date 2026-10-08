"""Per-user Windows MSI version mapping and checksum refresh.

MSI ProductVersion has three numeric fields, each at most 65535, and cannot
store a semver prerelease. Patch numbers through 64 are encoded as
``patch * 1000 + slot``. A stable release uses slot 999. ``X.Y.Z-rc.N`` uses
slot N (1 through 998). That orders ``0.3.29-rc.2`` (0.3.29002) before
``0.3.29`` (0.3.29999) and before ``0.3.30-rc.1`` (0.3.30001).
"""
import argparse
import hashlib
import json
import re
import shutil
import subprocess
from pathlib import Path
import uuid

from release_version import parse_release_version

UPGRADE_NAMESPACE = uuid.NAMESPACE_URL
UPGRADE_NAME = "https://github.com/moonbase2090/Prismattyc#windows-msi-upgrade-code"
# Stable for the life of the product. A new value would install beside the old one.
UPGRADE_CODE = str(uuid.uuid5(UPGRADE_NAMESPACE, UPGRADE_NAME))

SIGNING_ENV = (
    "AZURE_TENANT_ID",
    "AZURE_CLIENT_ID",
    "AZURE_SUBSCRIPTION_ID",
    "AZURE_ARTIFACT_SIGNING_ENDPOINT",
    "AZURE_ARTIFACT_SIGNING_ACCOUNT",
    "AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE",
)
SIGNING_SKIPPED = (
    "Windows code signing skipped: Azure Artifact Signing settings are absent."
)
_RC = re.compile(r"^rc\.(0|[1-9][0-9]*)$")
_MSI_FIELD_MAX = 65535
_PATCH_MAX = 64


def msi_product_version(version: str) -> str:
    """Return the numeric MSI ProductVersion for a release version."""
    full, base = parse_release_version(version)
    full = full.split("+", 1)[0]
    major, minor, patch = (int(part) for part in base.split("."))
    if patch > _PATCH_MAX:
        raise ValueError(
            f"MSI product version cannot encode patch {patch}; the maximum is {_PATCH_MAX}"
        )
    if full == base:
        slot = 999
    else:
        match = _RC.fullmatch(full[len(base) + 1 :])
        if match is None:
            raise ValueError("MSI packaging accepts X.Y.Z or X.Y.Z-rc.N only")
        slot = int(match.group(1))
        if slot < 1 or slot > 998:
            raise ValueError("release candidate number must be 1 through 998")
    third = patch * 1000 + slot
    if third > _MSI_FIELD_MAX:
        raise ValueError("MSI product version field exceeds 65535")
    return f"{major}.{minor}.{third}"


def build_msi(payload: Path, repo: Path, version: str, out_msi: Path) -> None:
    """Compile prismattyc.wxs. WiX's directory check only succeeds on Windows."""
    product_version = msi_product_version(version)
    wix = _wix_command()
    command = [
        wix,
        "build",
        str(repo / "scripts/release/prismattyc.wxs"),
        "-arch",
        "x64",
        "-d",
        f"ProductVersion={product_version}",
        "-d",
        f"SourceVersion={version}",
        "-d",
        f"Payload={payload}",
        "-d",
        f"Repo={repo}",
        "-o",
        str(out_msi),
    ]
    _run_wix(command, out_msi.with_suffix(".wix-build.log"))
    _run_wix([wix, "msi", "validate", str(out_msi)], out_msi.with_suffix(".wix-validate.log"))


def _wix_command() -> str:
    found = shutil.which("wix")
    if found:
        return found
    candidate = Path.home() / ".dotnet" / "tools" / "wix.exe"
    if candidate.is_file():
        return str(candidate)
    unix_candidate = Path.home() / ".dotnet" / "tools" / "wix"
    if unix_candidate.is_file():
        return str(unix_candidate)
    raise RuntimeError("WiX 5.0.2 is not installed. Run: dotnet tool install --global wix --version 5.0.2")


def _run_wix(command: list[str], log_path: Path) -> None:
    result = subprocess.run(command, capture_output=True, text=True)
    log_path.write_text(result.stdout + result.stderr, encoding="utf-8")
    if result.returncode != 0:
        raise RuntimeError(f"{' '.join(command[:3])} failed\n{result.stdout}\n{result.stderr}")


def signing_enabled(env: dict[str, str]) -> bool:
    """True only when every Artifact Signing setting is present and non-blank."""
    return all(env.get(name, "").strip() for name in SIGNING_ENV)


def refresh_checksums(out_dir: Path, *, signed: bool) -> None:
    """Recompute manifest asset hashes and SHA256SUMS-windows after signing."""
    out_dir = out_dir.resolve()
    manifests = sorted(out_dir.glob("manifest-*.json"))
    if len(manifests) != 1:
        raise ValueError(f"expected one manifest in {out_dir}, found {len(manifests)}")
    manifest_path = manifests[0]
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    assets = []
    for asset in manifest["assets"]:
        path = out_dir / asset["name"]
        if not path.is_file():
            raise ValueError(f"missing packaged asset {asset['name']}")
        assets.append(
            {"name": path.name, "size": path.stat().st_size, "sha256": _digest(path)}
        )
    msi_name = next((item["name"] for item in assets if item["name"].endswith(".msi")), None)
    if msi_name is None:
        msi = next(iter(sorted(out_dir.glob("*.msi"))), None)
        if msi is None:
            raise ValueError(f"no MSI in {out_dir}")
        assets.append({"name": msi.name, "size": msi.stat().st_size, "sha256": _digest(msi)})
    manifest["assets"] = assets
    manifest["signed"] = signed
    manifest["signing"] = "azure-artifact-signing" if signed else "unsigned"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    files = sorted(
        path
        for path in out_dir.iterdir()
        if path.is_file()
        and path.name != "SHA256SUMS-windows"
        and not path.name.endswith((".wix-build.log", ".wix-validate.log"))
    )
    (out_dir / "SHA256SUMS-windows").write_text(
        "".join(f"{_digest(path)}  {path.name}\n" for path in files),
        encoding="utf-8",
    )


def _digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--refresh-checksums", type=Path, dest="out_dir")
    parser.add_argument("--signed", action="store_true")
    args = parser.parse_args()
    if args.out_dir is None:
        parser.error("pass --refresh-checksums DIR")
    refresh_checksums(args.out_dir, signed=args.signed)


if __name__ == "__main__":
    main()
