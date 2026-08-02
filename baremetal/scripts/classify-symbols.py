#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
"""Partition _Data_/demangled_symbols.txt into code/data/uncertain.

Rules are expressed as ordered (pattern, verdict) pairs below. The
script walks each symbol through the rules in order; the first match
wins. If no rule matches, the symbol goes into `uncertain` and waits
for a new rule.

Usage:
    scripts/classify-symbols.py              # writes three files + summary
    scripts/classify-symbols.py --summary    # summary only
    scripts/classify-symbols.py --show code  # dump the code list

Output files (alongside this script, under scripts/classify-out/):
    code.txt        one symbol per line, "0xADDR\\tNAME"
    data.txt        same format
    uncertain.txt   same format — feed these back as new rules

The goal is to shrink `uncertain` to zero by refining the ruleset below.
"""

from __future__ import annotations
import argparse
import re
import struct
import sys
from dataclasses import dataclass
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent                # baremetal/
SYMBOLS_PATH = REPO_ROOT / "../_Data_/demangled_symbols.txt"
ROM_PATH = REPO_ROOT / "roms/newton.rom"
REX_PATH = REPO_ROOT / "../_Data_/Einstein.rex"
REX_PA_OFFSET = 0x0080_0000
ROM_APERTURE = 0x0100_0000  # 16 MiB ROM+REX aperture
OUT_DIR = SCRIPT_DIR / "classify-out"


def load_rom_words(rom_path: Path = ROM_PATH,
                   rex_path: Path = REX_PATH) -> list[int | None]:
    """Return 16 MiB ROM aperture as a word array (None = unmapped)."""
    words: list[int | None] = [None] * (ROM_APERTURE // 4)
    rom = rom_path.read_bytes()
    for i in range(len(rom) // 4):
        # MSB-first on disk → LE word the guest reads.
        words[i] = struct.unpack(">I", rom[i*4 : i*4+4])[0]
    rex = rex_path.read_bytes()
    rex_base = REX_PA_OFFSET // 4
    for i in range(len(rex) // 4):
        words[rex_base + i] = struct.unpack(">I", rex[i*4 : i*4+4])[0]
    return words


# Broad function-entry heuristic: cond=AL plus any recognised
# non-coprocessor ARM instruction encoding. Real Newton functions
# very commonly start with normal data-processing or load/store
# instructions (CMP, SUB, ADD, TEQ, LDR, STR, MOV, PUSH, ...), not
# just the narrow allowlist in classify-rom/src/main.rs.
#
# Excluded: cond != AL (function entries with conditional first
# insns exist — tail-called leaves — but the data-table false
# positives dominated by cond=EQ (top byte 0x00) force us to be
# strict here; add back conditional-entry support if a real
# example turns up), and top3=0b110/0b111 (coprocessor / SWI) —
# specific coproc encodings (MRC/MCR p15) are handled below.
def is_known_function_start(w: int | None) -> bool:
    if w is None:
        return False
    if (w >> 28) & 0xF != 0xE:                # AL only
        return False
    top3 = (w >> 25) & 0b111
    # 0b000: DP-reg / misc / extra load-store
    # 0b001: DP-immediate
    # 0b010: LDR/STR-immediate
    # 0b011: LDR/STR-register
    # 0b100: LDM/STM
    # 0b101: B / BL
    if top3 <= 0b101:
        return True
    # 0b110: coproc LDC/STC (rare as first insn; skip)
    # 0b111 with bit24=1: SWI — Newton has many one-liner SWI glues
    #                     (`SWI #n; MOV pc, lr`) that start with SWI.
    # 0b111 with bit24=0: coproc MRC/MCR — legitimate p15 low-level
    #                     prologue (cache / MMU setup, etc.)
    if top3 == 0b111:
        return True
    return False


# ------------------------------------------------------------------
# Rules. Walk in order; first matching rule wins.
#
# verdict ∈ {"code", "data", "drop"}
#   "code"  — function/entry that the classifier walker should seed
#   "data"  — global/table/constant the walker must NOT seed
#   "drop"  — linker-synthesised or otherwise not-a-real-symbol;
#             neither code nor data, just exclude from every list
# ------------------------------------------------------------------

@dataclass(frozen=True)
class Rule:
    name: str
    verdict: str
    matcher: object  # callable(addr: int, name: str, words: list[int|None]) -> bool


def name_starts_with_re(pattern: str):
    r = re.compile(pattern)
    return lambda addr, name, words: bool(r.match(name))


def name_contains_re(pattern: str):
    r = re.compile(pattern)
    return lambda addr, name, words: bool(r.search(name))


def name_contains(substr: str):
    return lambda addr, name, words: substr in name


def name_ends_with(suffix: str):
    return lambda addr, name, words: name.endswith(suffix)


def addr_in(lo: int, hi: int):
    """Matches addr in [lo, hi). hi is exclusive."""
    return lambda addr, name, words: lo <= addr < hi


def first_word_is_prologue(addr, name, words):
    idx = addr >> 2
    if idx >= len(words):
        return False
    return is_known_function_start(words[idx])


# ------------------------------------------------------------------
# Explicit exception lists.
#
# Keyed by address. Evaluated before any rule below — if an address
# appears in one of these sets it's classified immediately. Use for
# weird outliers that don't fit any general pattern; avoid writing
# contorted rules to catch single symbols.
#
# Populate as boot-time problems surface.
# ------------------------------------------------------------------

CODE_EXCEPTIONS: set[int] = set()
DATA_EXCEPTIONS: set[int] = set()

# Contiguous data-only address ranges. Evaluated early (after the
# exception sets) and override every other rule. Use for regions
# where the ROM interleaves dozens of tables and constants with no
# code between them — a single range is easier to maintain than a
# long address list.
#
# Current ranges:
#   0x00366f2c..0x00382324  recognition tables + modem constants +
#     DES tables + IrDA tables + charsets + yacc tables + C runtime
#     globals + QuickDraw pattern data. Verified (via bucket
#     cross-check) to contain zero code symbols.
DATA_RANGES: list[tuple[int, int]] = [
    (0x00366f2c, 0x00382324),
    # PublicFiller (0x003948e4) + bpWeight (0x003948f0) + a
    # ~90 KiB anonymous back-propagation weight table + the
    # newtConnects data table at 0x003aace4 — code resumes at
    # TCountXrAsm (0x003ad244).
    (0x003948e4, 0x003ad244),
    # Inline "SVC mode in MonitorEntryGlue\0" ASCII string embedded
    # at the tail of MonitorEntryGlue (0x00394318). The walker falls
    # through into it because the preceding DebuggerUND call doesn't
    # terminate the basic block cleanly.
    (0x0039433c, 0x0039435c),
]


def addr_in_any_range(ranges):
    return lambda addr, name, words: any(lo <= addr < hi for lo, hi in ranges)


def addr_in_set(s: set[int]):
    return lambda addr, name, words: addr in s


def first_word_top_byte_zero(addr, name, words):
    """True when the word at `addr` has high byte 0x00 — i.e. cond=EQ
    with a tiny immediate/shift field. Data tables (offsets, counts,
    pointers with MSB=0) dominate this pattern; real ARM code almost
    never starts here (verified: of 18,926 code symbols classified by
    name/address rules, only `CopyValue` at 0x371c94 had a 0x00_______
    first word, and it turned out to be mis-labelled data).
    """
    idx = addr >> 2
    if idx >= len(words) or words[idx] is None:
        return False
    return (words[idx] >> 24) == 0x00


RULES: list[Rule] = [
    # ---- explicit exceptions (highest priority) -----------------
    Rule("code-exception",  "code", addr_in_set(CODE_EXCEPTIONS)),
    Rule("data-exception",  "data", addr_in_set(DATA_EXCEPTIONS)),
    Rule("data-range",      "data", addr_in_any_range(DATA_RANGES)),

    # ---- drop linker markers / section synthetic symbols --------
    Rule("linker-$$",       "drop", name_contains("$$")),
    Rule("linker-Image$",   "drop", name_starts_with_re(r"Image\$")),
    Rule("linker-$Size",    "drop", name_ends_with("$Size")),
    Rule("linker-$Length",  "drop", name_ends_with("$Length")),
    Rule("linker-$Base",    "drop", name_ends_with("$Base")),
    Rule("linker-$Limit",   "drop", name_ends_with("$Limit")),
    Rule("linker-$End",     "drop", name_ends_with("$End")),
    Rule("linker-$ZI",      "drop", name_ends_with("$ZI")),

    # ---- address-range DATA rule (must come before code ranges)
    # Exception vectors + early low-ROM globals: data, not seedable
    # as code roots. The real vector dispatch starts at ROMBoot
    # (0x18688); the 8 vector entries live at 0x00..0x1C and the
    # region up to 0x200 holds boot globals the classifier must not
    # follow.
    Rule("addr<0x200",      "data", addr_in(0x0000_0000, 0x0000_0200)),

    # ---- definitely data (name-based) --------------------------
    # Global / constant prefixes have to win over address-range
    # code rules, because the early-boot range [0x18400, 0x1A260)
    # is interleaved with `g*` globals (gInitialCPUMode,
    # gDiagCheckTag1, gPhysROMAcsum, ...).
    Rule("gPrefix",         "data", name_starts_with_re(r"g[A-Z]")),
    Rule("kPrefix",         "data", name_starts_with_re(r"k[A-Z]")),
    # Newton localization / symbol / ratio tables seen in
    # fallback-data buckets of earlier iterations.
    Rule("SYM-table",       "data", name_starts_with_re(r"SYM[a-zA-Z_]")),
    Rule("RSSYM-table",     "data", name_starts_with_re(r"RSSYM[a-zA-Z_]")),
    Rule("rat-table",       "data", name_starts_with_re(r"rat[0-9]")),
    Rule("BiSL-table",      "data", name_starts_with_re(r"BiS[LP]")),
    Rule("BiGS-table",      "data", name_starts_with_re(r"BiGS")),
    Rule("BiG-table",       "data", name_starts_with_re(r"BiG[A-Z]")),
    Rule("big-table",       "data", name_starts_with_re(r"big(\b|[A-Z])")),
    Rule("the-prefix",      "data", name_starts_with_re(r"the[A-Z]")),
    # ROMGList, ROMGrammar and other ROM* data labels (not the
    # ROMBoot function which is in addr-early-boot).
    Rule("ROMG-table",      "data", name_starts_with_re(r"ROMG")),
    # Recognition-subsystem tables: ros* (ROS = recognition object
    # subsystem), raw* image data, bp* (back-propagation neural
    # net params), ar* (arithmetic coding state).
    Rule("ros-table",       "data", name_starts_with_re(r"ros[A-Z]")),
    Rule("raw-table",       "data", name_starts_with_re(r"raw[A-Za-z0-9]")),
    Rule("bp-table",        "data", name_starts_with_re(r"bp[A-Z]")),
    Rule("arBP-table",      "data", name_starts_with_re(r"arBP")),
    # AT modem command strings: `cmdFClass` = "+FCL", `cmdEscape2CmdMode`
    # = "+++\0", etc. All have first words matching ASCII byte
    # patterns, not instructions.
    Rule("cmd-prefix",      "data", name_starts_with_re(r"cmd[A-Z]")),
    # Arithmetic-coding / signal lookup tables: ArProbEncodeLu1,
    # ArProbDecodeLu, ArSigLu, ArSigSlopeLu, QSigLu.
    Rule("Lu-suffix",       "data", name_starts_with_re(r"(Ar|Q)[A-Za-z]*Lu")),

    # ---- address-range CODE rule (after name-based data rules) -
    # Early boot code: every word in [0x18400, 0x1A260) that wasn't
    # caught by a data-name rule is a function entry of the
    # hand-rolled assembly init path (FlushTheCache, ROMBoot,
    # SetFIQStack, ...). Seeds the walker directly from those
    # entry points even when the demangled name doesn't match any
    # other rule.
    Rule("addr-early-boot", "code", addr_in(0x0001_8400, 0x0001_A260)),

    # ---- definitely code (name-based) --------------------------
    Rule("has-::",          "code", name_contains("::")),
    Rule("has-(",           "code", name_contains("(")),
    # Classic C++ mangled names: e.g. `Foo__12SomeClassIv`,
    # `f__3FooFi`, `__ct__5TFooFv`. The tell is a `__` followed by
    # a digit, which encodes the mangled length of the class name.
    Rule("mangled-__N",     "code", name_contains_re(r"__\d")),
    # Newton "F" function convention: `FFoo`, `FFlushTheCache`,
    # `FTimeInSeconds`, ... — capital F followed by another capital.
    # All 852 F[A-Z] matches have AL-cond first words; the convention
    # is reliable so we accept purely on name.
    Rule("F[A-Z]",          "code", name_starts_with_re(r"F[A-Z]")),

    # ---- definitely code (C-function-name conventions) --------
    # Catch C function names the demangler doesn't decorate (no
    # `::` / `(`). Prefix set is the list of common action verbs
    # Newton's C codebase uses; expand as fallbacks surface more.
    #
    # Requires the first word to also look like an AL-conditional
    # prologue: a data table that coincidentally has one of these
    # name prefixes (e.g., `CopyValue` at 0x371c94 with first word
    # 0x00000000, sitting between `LLBase` and `LZCopyBits` data
    # labels) would otherwise be wrongly coded.
    Rule("C-verb-prefix",   "code", lambda a, n, w:
        name_starts_with_re(
            r"(Get|Set|Make|New|Free|Add|Delete|Copy|Init|Check|Verify|Build|Compute|Handle|Find|Is|Has|Next|First|Last)[A-Z]")(a, n, w)
        and first_word_is_prologue(a, n, w)),

    # ---- fallback: inspect the first word ---------------------
    # If the first word decodes as a recognised function prologue,
    # classify as code; otherwise fall through to the data catch.
    Rule("prologue=yes",    "code", first_word_is_prologue),

    # ---- catch-all data shape ---------------------------------
    # First-word high byte 0x00 means cond=EQ with a tiny magnitude
    # field — the dominant shape of data-table entries (counts,
    # offsets, MSB-zero pointers). See `first_word_top_byte_zero`
    # docstring for why this is a safe blanket.
    Rule("first-word=0x00", "data", first_word_top_byte_zero),
]


# ------------------------------------------------------------------
# Driver
# ------------------------------------------------------------------

def load_symbols(path: Path) -> list[tuple[int, str]]:
    out = []
    for raw in path.read_text().splitlines():
        parts = raw.split("\t", 1)
        if len(parts) != 2:
            continue
        addr_s, name = parts[0].strip(), parts[1].strip()
        if not addr_s or not name:
            continue
        try:
            addr = int(addr_s, 16) if addr_s.lower().startswith("0x") else int(addr_s, 16)
        except ValueError:
            continue
        if addr & 3 != 0:
            continue
        if addr >= 0x0100_0000:
            continue
        out.append((addr, name))
    return out


def classify(addr: int, name: str, words: list[int | None]) -> tuple[str, str]:
    for r in RULES:
        if r.matcher(addr, name, words):
            return r.verdict, r.name
    return "uncertain", "-"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--summary", action="store_true", help="only print counts")
    ap.add_argument("--show", choices=["code", "data", "uncertain", "drop"],
                    help="dump this bucket to stdout instead of writing files")
    ap.add_argument("--symbols", default=str(SYMBOLS_PATH),
                    help="path to demangled_symbols.txt")
    ap.add_argument("--rom", default=str(ROM_PATH),
                    help="path to the ROM image")
    ap.add_argument("--rex", default=str(REX_PATH),
                    help="path to the REx image")
    ap.add_argument("--code-symbols-out", default=str(OUT_DIR / "code-symbols.txt"),
                    help="where to write the curated code-only list "
                         "(build.rs symbol-table / tracer input)")
    args = ap.parse_args()

    syms = load_symbols(Path(args.symbols))
    if not syms:
        print(f"no symbols loaded from {args.symbols}", file=sys.stderr)
        return 1
    words = load_rom_words(Path(args.rom), Path(args.rex))

    buckets: dict[str, list[tuple[int, str, str]]] = {
        "code": [], "data": [], "uncertain": [], "drop": [],
    }
    for addr, name in syms:
        v, rule = classify(addr, name, words)
        buckets[v].append((addr, name, rule))

    if args.show:
        for addr, name, rule in buckets[args.show]:
            print(f"0x{addr:08x}\t{name}\t[{rule}]")
        return 0

    if not args.summary:
        OUT_DIR.mkdir(exist_ok=True)
        for bucket, entries in buckets.items():
            path = OUT_DIR / f"{bucket}.txt"
            with path.open("w") as f:
                for addr, name, rule in entries:
                    f.write(f"0x{addr:08x}\t{name}\t[{rule}]\n")

        # Also emit a 2-column code list in the same format as
        # _Data_/demangled_symbols.txt so classify-rom can consume
        # it directly via --symbols. Keeps classify-rom's root
        # selection out of the symbol-classification business.
        code_syms_path = Path(args.code_symbols_out)
        code_syms_path.parent.mkdir(parents=True, exist_ok=True)
        with code_syms_path.open("w") as f:
            for addr, name, _ in buckets["code"]:
                f.write(f"0x{addr:08X}\t{name}\n")


    total = sum(len(v) for v in buckets.values())
    print(f"total symbols scanned: {total}")
    for k in ("code", "data", "uncertain", "drop"):
        print(f"  {k:10s} {len(buckets[k]):6d}")

    # Per-rule breakdown: shows how many symbols each rule caught.
    # Useful for spotting a rule that's catching unexpected things.
    print("\nper-rule hits:")
    rule_order = [r.name for r in RULES]
    rule_counts: dict[str, tuple[str, int]] = {}
    for bucket, entries in buckets.items():
        for _, _, rule_name in entries:
            verdict, cnt = rule_counts.get(rule_name, (bucket, 0))
            rule_counts[rule_name] = (verdict, cnt + 1)
    for rn in rule_order:
        if rn in rule_counts:
            v, c = rule_counts[rn]
            print(f"  {rn:20s} -> {v:4s}  {c:6d}")

    if not args.summary:
        print(f"\nwrote {OUT_DIR}/{{code,data,uncertain,drop}}.txt")
        print(f"wrote {args.code_symbols_out}")
        if buckets["uncertain"]:
            print("\nfirst 30 uncertain (propose rules for these):")
            for addr, name, _ in buckets["uncertain"][:30]:
                print(f"  0x{addr:08x}  {name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
