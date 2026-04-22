#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
"""Dump the first 2 words at each address in `uncertain.txt` (or a
supplied list), formatted so the set of "first words" that should
be in the prologue allowlist is easy to eyeball.

Usage:
    show-first-word.py                # first words of uncertain.txt
    show-first-word.py --limit 40
    show-first-word.py --list foo.txt # another input list
    show-first-word.py --raw 0x30c3c  # just one address
"""

from __future__ import annotations
import argparse
import struct
import sys
from collections import Counter
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
ROM_PATH = REPO_ROOT / "roms/newton.rom"
REX_PATH = REPO_ROOT / "../_Data_/Einstein.rex"
REX_PA_OFFSET = 0x0080_0000
UNCERTAIN = SCRIPT_DIR / "classify-out/uncertain.txt"


def load_rom():
    rom = ROM_PATH.read_bytes()
    rex = REX_PATH.read_bytes()
    total = bytearray(16 * 1024 * 1024)
    total[:len(rom)] = rom
    total[REX_PA_OFFSET:REX_PA_OFFSET + len(rex)] = rex
    return bytes(total)


def read_word_be_as_le(b: bytes, addr: int) -> int | None:
    if addr + 4 > len(b):
        return None
    return struct.unpack(">I", b[addr:addr + 4])[0]


def disasm_hint(w: int) -> str:
    """Coarse decode for eyeballing — cond + op group only."""
    cond = (w >> 28) & 0xF
    cond_s = ["EQ","NE","CS","CC","MI","PL","VS","VC",
              "HI","LS","GE","LT","GT","LE","AL","NV"][cond]
    top = (w >> 25) & 0b111
    sub = {
        0b000: "DP/misc/extraldst",
        0b001: "DP-imm",
        0b010: "LDR/STR-imm",
        0b011: "LDR/STR-reg (bit4=0) / media",
        0b100: "LDM/STM",
        0b101: "B/BL",
        0b110: "coproc LDC/STC",
        0b111: "coproc MRC/MCR / SWI",
    }[top]
    # Specific common-prologue tells
    if (w & 0x0FFF_0000) == 0x092D_0000: return f"{cond_s} PUSH {{...}}"
    if (w & 0x0FFF_F000) == 0x024D_D000: return f"{cond_s} SUB sp,sp,#imm"
    if w == 0xE52D_E004:                  return f"{cond_s} STR lr,[sp,#-4]!"
    if w == 0xE1A0_C00D:                  return f"{cond_s} MOV ip,sp"
    if (w & 0x0FFF_0000) == 0x03A0_0000: return f"{cond_s} MOV Rd,#imm"
    if (w & 0x0FFF_0000) == 0x03E0_0000: return f"{cond_s} MVN Rd,#imm"
    if (w & 0x0FFF_0FF0) == 0x01A0_0000: return f"{cond_s} MOV Rd,Rn"
    if (w & 0x0FFF_F000) == 0x059F_0000: return f"{cond_s} LDR Rd,[pc,#imm]"
    if (w & 0x0F00_0000) == 0x0A00_0000: return f"{cond_s} B/BL target"
    # DP-imm encoding: cond 001 opcode S Rn Rd imm12
    if top == 0b001:
        opc = (w >> 21) & 0xF
        rd = (w >> 12) & 0xF
        rn = (w >> 16) & 0xF
        opname = ["AND","EOR","SUB","RSB","ADD","ADC","SBC","RSC",
                  "TST","TEQ","CMP","CMN","ORR","MOV","BIC","MVN"][opc]
        return f"{cond_s} {opname} r{rd},r{rn},#imm"
    if top == 0b010:
        # LDR/STR imm
        l = (w >> 20) & 1
        b_bit = (w >> 22) & 1
        return f"{cond_s} {'LDR' if l else 'STR'}{'B' if b_bit else ''} imm"
    if top == 0b100:
        l = (w >> 20) & 1
        return f"{cond_s} {'LDM' if l else 'STM'}"
    return f"{cond_s} {sub}"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--list", default=str(UNCERTAIN))
    ap.add_argument("--limit", type=int, default=0,
                    help="stop after this many entries (0 = all)")
    ap.add_argument("--raw", action="append", default=[],
                    help="dump only these addresses (hex), ignoring --list")
    ap.add_argument("--histogram", action="store_true",
                    help="show a histogram of first-word disassembly hints")
    args = ap.parse_args()

    rom = load_rom()
    if args.raw:
        entries = [(int(a, 16), a) for a in args.raw]
        for addr, raw in entries:
            w0 = read_word_be_as_le(rom, addr)
            w1 = read_word_be_as_le(rom, addr + 4)
            if w0 is None:
                print(f"0x{addr:08x}\t(out of range)\t{raw}")
                continue
            print(f"0x{addr:08x}  {w0:08x} {w1 or 0:08x}  {disasm_hint(w0)}")
        return 0

    hist: Counter[str] = Counter()
    shown = 0
    for line in Path(args.list).read_text().splitlines():
        parts = line.split("\t")
        if not parts or not parts[0].startswith("0x"):
            continue
        addr = int(parts[0], 16)
        name = parts[1] if len(parts) > 1 else ""
        w0 = read_word_be_as_le(rom, addr)
        if w0 is None:
            continue
        hint = disasm_hint(w0)
        hist[hint] += 1
        if not args.histogram:
            if args.limit and shown >= args.limit:
                break
            w1 = read_word_be_as_le(rom, addr + 4) or 0
            print(f"0x{addr:08x}  {w0:08x} {w1:08x}  {hint:40s}  {name}")
            shown += 1

    if args.histogram:
        print(f"{'count':>6}  hint")
        for hint, cnt in hist.most_common():
            print(f"{cnt:>6}  {hint}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
