"""Validation shared by platform-specific release packagers."""
import re


_NUMBER = r"(?:0|[1-9][0-9]*)"
_PRERELEASE_ID = r"(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
_BUILD_ID = r"[0-9A-Za-z-]+"
_VERSION = re.compile(
    rf"^(?P<base>{_NUMBER}\.{_NUMBER}\.{_NUMBER})"
    rf"(?:-(?P<prerelease>{_PRERELEASE_ID}(?:\.{_PRERELEASE_ID})*))?"
    rf"(?:\+(?:{_BUILD_ID}(?:\.{_BUILD_ID})*))?$"
)


def parse_release_version(value: str) -> tuple[str, str]:
    """Return ``(full_version, base_version)`` or reject a malformed version."""
    if value.startswith("v"):
        value = value[1:]
    match = _VERSION.fullmatch(value)
    if match is None:
        raise ValueError("version must be semver X.Y.Z with an optional prerelease/build suffix")
    return value, match.group("base")


def reports_release_version(output: str, value: str) -> bool:
    """Accept an exact tag version or its base version for prerelease builds."""
    full, base = parse_release_version(value)
    reported = set(output.split())
    return full in reported or base in reported
