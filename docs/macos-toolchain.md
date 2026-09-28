# macOS toolchain preflight

One machine's toolchain check after the macOS 27 upgrade, kept out of the port inventory (see docs/macos.md).

## macOS 27 local toolchain

The local preflight was checked on 2026-09-26 after the macOS 27 upgrade:

| Check | Result |
| --- | --- |
| OS | macOS 27.0, build 26A428, Apple silicon |
| Active developer directory | `/Library/Developer/CommandLineTools` |
| Command Line Tools package | 27.0 (`27.0.0.0.1788430756`) |
| Active macOS SDK | 27.0 at `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk` |
| Linker | `/Library/Developer/CommandLineTools/usr/bin/ld` |
| Software Update | No updates available at check time |
| Free data volume space | 1.7 TiB |

This is the matching toolchain for the macOS 27 SDK. The earlier mismatch
between the installed SDK and older Command Line Tools is resolved. No Prismattyc
build was run as part of this preflight, and backup status was not checked.

To repeat the toolchain check:

```bash
sw_vers
uname -m
xcode-select -p
pkgutil --pkg-info=com.apple.pkg.CLTools_Executables
xcrun --find ld
xcrun --sdk macosx --show-sdk-version
xcrun --sdk macosx --show-sdk-path
```

Apple's [Xcode system requirements](https://developer.apple.com/xcode/system-requirements)
list the OS versions supported by each Xcode release. For a macOS upgrade,
Apple recommends a backup before using
[Software Update](https://support.apple.com/en-us/127455).

