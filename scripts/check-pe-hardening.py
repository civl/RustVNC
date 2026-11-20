#!/usr/bin/env python3
"""Verify the shipped Windows binary really carries the M3 hardening flags.

The linker flags live in `.cargo/config.toml`, but a build that silently drops
them (different target triple, overridden rustflags, a CI cache surprise) would
still look fine in review. This asserts on the produced PE instead of on intent:

    python3 scripts/check-pe-hardening.py target/release/rustvnc-client.exe

Exits non-zero if any required bit is missing, so CI fails on a regression.
"""

from __future__ import annotations

import struct
import sys

# IMAGE_DLLCHARACTERISTICS_*
REQUIRED = {
    "HIGH_ENTROPY_VA": 0x0020,
    "DYNAMIC_BASE (ASLR)": 0x0040,
    "NX_COMPAT (DEP)": 0x0100,
    "GUARD_CF": 0x4000,
    "CET_COMPAT": 0x8000,
}

IMPORTANT_OPTIONAL = {"FORCE_INTEGRITY": 0x0080}


def main(path: str) -> int:
    data = open(path, "rb").read()
    if data[:2] != b"MZ":
        print(f"{path}: not a PE file")
        return 2
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if data[e_lfanew : e_lfanew + 4] != b"PE\0\0":
        print(f"{path}: missing PE signature")
        return 2
    coff = e_lfanew + 4
    num_sections = struct.unpack_from("<H", data, coff + 2)[0]
    opt_size = struct.unpack_from("<H", data, coff + 16)[0]
    opt = coff + 20
    magic = struct.unpack_from("<H", data, opt)[0]
    if magic != 0x20B:
        print(f"{path}: expected a PE32+ image, found magic 0x{magic:x}")
        return 2

    dll_char = struct.unpack_from("<H", data, opt + 70)[0]
    failures = []
    for name, bit in REQUIRED.items():
        present = bool(dll_char & bit)
        print(f"  {name:<22} {'YES' if present else 'MISSING'}")
        if not present:
            failures.append(name)

    # CFG also needs a load config directory; the header bit alone is decoration.
    load_config_rva, load_config_size = struct.unpack_from("<II", data, opt + 112 + 10 * 8)
    print(f"  Load Config            rva=0x{load_config_rva:x} size={load_config_size}")
    if load_config_size == 0:
        failures.append("Load Config directory")

    if failures:
        print(f"FAIL: {path} is missing {', '.join(failures)}")
        return 1

    print(f"OK: {path} carries ASLR, DEP, HighEntropyVA, CFG and CET compatibility")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1]))
