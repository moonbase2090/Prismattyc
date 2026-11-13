"""Per-user Windows MSI version mapping and checksum refresh.

MSI ProductVersion is ``major.minor.build``. Major and minor are at most 255
and build is at most 65535. It cannot store a semver prerelease, so every
release also gets a slot: a stable release uses the top slot and
``X.Y.Z-rc.N`` uses slot N, which keeps a candidate before its stable release.

Two layouts exist, because 0.3.0 through 0.3.64 already shipped (or were
tagged) with the first one and a newer MSI must always sort after an older one.

Legacy, for 0.x releases up to 0.3.64: ``0.Y.patch*1000+slot``. Stable slot
999, candidate slot 1 through 998. ``0.3.29-rc.2`` is ``0.3.29002``, ``0.3.29``
is ``0.3.29999`` and ``0.3.64`` is ``0.3.64999``. It cannot go past patch 64
because 65999 is the last build value that fits.

Current, for everything after 0.3.64: ``(major+1).minor.patch*100+slot``. Stable
slot 99, candidate slot 1 through 98. ``0.3.65`` is ``1.3.6599``, ``0.4.0`` is
``1.4.99`` and ``1.0.0`` is ``2.0.99``. The major field starts at 1, so every
current value sorts after every legacy value (which all have major 0). This
reaches patch 654 and major 254.
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
# WixQuietExec, which copies the pmuxd start script's output into the MSI log.
# Pinned to the WiX release in build-windows.ps1.
WIX_UTIL_EXTENSION = "WixToolset.Util.wixext/5.0.2"
_RC = re.compile(r"^rc\.(0|[1-9][0-9]*)$")
_MSI_BUILD_MAX = 65535
_MSI_MAJOR_MINOR_MAX = 255
# Last release that keeps the legacy layout. Anything later sorts above it by
# using a higher MSI major field.
_LEGACY_LAST = (0, 3, 64)


def msi_product_version(version: str) -> str:
    """Return the numeric MSI ProductVersion for a release version."""
    full, base = parse_release_version(version)
    full = full.split("+", 1)[0]
    major, minor, patch = (int(part) for part in base.split("."))
    candidate = None
    if full != base:
        match = _RC.fullmatch(full[len(base) + 1 :])
        if match is None:
            raise ValueError("MSI packaging accepts X.Y.Z or X.Y.Z-rc.N only")
        candidate = int(match.group(1))
    if (major, minor, patch) <= _LEGACY_LAST:
        # Releases before 0.3.65: the layout the shipped installers use.
        if major != 0 or patch > 64:
            raise ValueError(
                f"MSI product version cannot encode {base}: before 0.3.65 only 0.Y.Z with Z up to 64 exists"
            )
        slot_max, stable_slot, scale = 998, 999, 1000
        out_major = major
    else:
        slot_max, stable_slot, scale = 98, 99, 100
        out_major = major + 1
    if candidate is None:
        slot = stable_slot
    else:
        slot = candidate
        if slot < 1 or slot > slot_max:
            raise ValueError(f"release candidate number must be 1 through {slot_max}")
    build = patch * scale + slot
    if out_major > _MSI_MAJOR_MINOR_MAX or minor > _MSI_MAJOR_MINOR_MAX:
        raise ValueError(f"MSI product version cannot encode {base}: major or minor is too large")
    if build > _MSI_BUILD_MAX:
        raise ValueError(f"MSI product version cannot encode {base}: patch is too large")
    return f"{out_major}.{minor}.{build}"


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
        "-ext",
        WIX_UTIL_EXTENSION,
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
