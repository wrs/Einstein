#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
"""Walk the classifier's reach.bitmap and emit hex dumps of every
contiguous DATA region (reach bit = 0) and every contiguous CODE
region (reach bit = 1).

Output is BE numerical view — what a CPSR.E=1 LDR returns, and the
ARM instruction encoding pre-byteswap — 4 words per line, with ASCII
gloss and a heuristic annotation when a word looks like an in-ROM
function pointer.

Pure-zero / pure-0xFFFFFFFF runs collapse to one summary line.

Files written alongside the bitmap, under classify/<hash>/:
    data-regions.txt — runs of bit=0 (data the kernel must read raw)
    code-regions.txt — runs of bit=1 (code that gets byteswapped at
                       load time so LE instruction fetch decodes it)
"""
from __future__ import annotations
import argparse
import struct
import sys
from pathlib import Path

ROM_SIZE = 0x100_0000  # 16 MiB aperture (newton.rom + Einstein.rex window)
WORDS_PER_LINE = 4


def find_bitmap(classify_dir: Path) -> Path:
    candidates = [d for d in classify_dir.iterdir() if (d / "reach.bitmap").is_file()]
    if not candidates:
        raise SystemExit(f"no reach.bitmap under {classify_dir}")
    candidates.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return candidates[0] / "reach.bitmap"


def load_rom_aperture(rom_path: Path, rex_path: Path) -> bytes:
    rom = rom_path.read_bytes()
    rex = rex_path.read_bytes()
    buf = bytearray(ROM_SIZE)
    buf[0:len(rom)] = rom
    buf[0x80_0000:0x80_0000 + len(rex)] = rex
    return bytes(buf)


def reach_bit(bitmap: bytes, idx: int) -> int:
    """Return the reach bit (0 = data, 1 = code) for word index `idx`."""
    byte = idx >> 3
    bit = idx & 7
    if byte >= len(bitmap):
        return 0
    return (bitmap[byte] >> bit) & 1


def find_runs(bitmap: bytes, total_words: int, target_bit: int) -> list[tuple[int, int]]:
    """Return [(start_word_idx, end_word_idx_exclusive), ...] for every
    contiguous run of words whose reach bit equals `target_bit`."""
    runs: list[tuple[int, int]] = []
    i = 0
    while i < total_words:
        if reach_bit(bitmap, i) == target_bit:
            j = i
            while j < total_words and reach_bit(bitmap, j) == target_bit:
                j += 1
            runs.append((i, j))
            i = j
        else:
            i += 1
    return runs


def is_pa_in_rom(value: int) -> bool:
    return 0 < value < ROM_SIZE and (value & 3) == 0


def is_pa_in_rex(value: int) -> bool:
    return 0x80_0000 <= value < 0x100_0000 and (value & 3) == 0


def ascii_gloss(words: list[int]) -> str:
    out = []
    for w in words:
        for shift in (24, 16, 8, 0):
            b = (w >> shift) & 0xFF
            out.append(chr(b) if 32 <= b < 127 else ".")
    return "".join(out)


def suspicion_tag(words: list[int], is_code: bool) -> str:
    """For a code region, return '' if all instructions look plausible
    or ' SUSPICIOUS: ...' otherwise. A region is suspicious if it
    contains any NV-cond word (cond=0xF, deprecated in ARMv7+, never
    real code) or more than 25% non-AL-cond words. Data regions don't
    get the tag — non-AL words are normal data shapes."""
    if not is_code or not words:
        return ""
    nv = sum(1 for w in words if (w >> 28) & 0xF == 0xF)
    non_al = sum(1 for w in words if (w >> 28) & 0xF != 0xE)
    flags: list[str] = []
    if nv:
        flags.append(f"{nv} NV-cond")
    pct = 100.0 * non_al / len(words)
    if pct > 25.0:
        flags.append(f"{non_al}/{len(words)} non-AL ({pct:.1f}%)")
    return f"  SUSPICIOUS: {', '.join(flags)}" if flags else ""


def dump_region(
    aperture: bytes,
    start_word: int,
    end_word: int,
    f,
    is_code: bool = False,
) -> None:
    start_pa = start_word * 4
    end_pa = end_word * 4
    word_count = end_word - start_word
    byte_count = word_count * 4

    # Read all words for this region (BE numerical).
    words = list(struct.unpack(f">{word_count}I", aperture[start_pa:end_pa]))

    suspicion = suspicion_tag(words, is_code)
    print(f"{byte_count} bytes{suspicion}", file=f)
    if all(w == 0 for w in words):
        print(f"  {start_pa:08x}: (zero fill)", file=f)
        return
    if all(w == 0xFFFF_FFFF for w in words):
        print(f"  {start_pa:08x}: (0xFFFFFFFF fill)", file=f)
        return

    # Collapse leading and trailing zero runs.
    head_zeros = 0
    while head_zeros < word_count and words[head_zeros] == 0:
        head_zeros += 1
    tail_zeros = 0
    while tail_zeros < word_count - head_zeros and words[word_count - 1 - tail_zeros] == 0:
        tail_zeros += 1
    if head_zeros >= 4:
        print(
            f"  {start_pa:08x}: (zero fill, {head_zeros*4} bytes)",
            file=f,
        )

    body_start = head_zeros
    body_end = word_count - tail_zeros

    # Word-hex with ASCII gloss, WORDS_PER_LINE per line. Annotate words
    # that look like in-ROM PAs (potential function pointer literals).
    i = body_start
    while i < body_end:
        chunk_end = min(i + WORDS_PER_LINE, body_end)
        chunk = words[i:chunk_end]
        line_pa = start_pa + i * 4
        hex_part = " ".join(f"{w:08x}" for w in chunk)
        # Pad short final line so columns line up.
        hex_part = hex_part.ljust(WORDS_PER_LINE * 9 - 1)
        ascii_part = ascii_gloss(chunk)
        annotations: list[str] = []
        for off, w in enumerate(chunk):
            if is_pa_in_rom(w) and not is_pa_in_rex(w):
                annotations.append(f"+{off*4:02x}=ROM_PA")
            elif is_pa_in_rex(w):
                annotations.append(f"+{off*4:02x}=REX_PA")
        ann = ("  // " + " ".join(annotations)) if annotations else ""
        print(f"  {line_pa:08x}:  {hex_part}  |{ascii_part}|{ann}", file=f)
        i = chunk_end

    if tail_zeros >= 4:
        print(
            f"  {start_pa + body_end*4:08x}: (zero fill, {tail_zeros*4} bytes)",
            file=f,
        )


def write_report(
    out_path: Path,
    label: str,
    rom_path: Path,
    rex_path: Path,
    bitmap_path: Path,
    aperture: bytes,
    total_words: int,
    runs: list[tuple[int, int]],
) -> None:
    is_code = (label == "code")
    with out_path.open("w") as f:
        print(f"# {label}-regions report", file=f)
        print(f"# rom:     {rom_path}", file=f)
        print(f"# rex:     {rex_path}", file=f)
        print(f"# bitmap:  {bitmap_path}", file=f)
        print(
            f"# {len(runs)} contiguous {label} regions across "
            f"{total_words} words ({total_words*4} bytes)",
            file=f,
        )
        total = sum(e - s for s, e in runs)
        print(
            f"# {total} {label} words "
            f"({total*4} bytes, "
            f"{100.0*total/total_words:.1f}% of aperture)",
            file=f,
        )
        if is_code:
            print(
                f"# A region is tagged SUSPICIOUS if it contains any "
                f"NV-cond instruction (cond=0xF) or more than 25% non-AL "
                f"instructions — likely false-positive code marks.",
                file=f,
            )
        print(file=f)
        for start_word, end_word in runs:
            dump_region(aperture, start_word, end_word, f, is_code=is_code)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rom", type=Path, default=Path("roms/newton.rom"))
    ap.add_argument("--rex", type=Path, default=Path("../_Data_/Einstein.rex"))
    ap.add_argument("--classify-dir", type=Path, default=Path("classify"))
    args = ap.parse_args()

    here = Path(__file__).resolve().parent.parent
    rom_path = (here / args.rom).resolve()
    rex_path = (here / args.rex).resolve()
    classify_dir = (here / args.classify_dir).resolve()

    bitmap_path = find_bitmap(classify_dir)
    bitmap = bitmap_path.read_bytes()
    aperture = load_rom_aperture(rom_path, rex_path)
    total_words = ROM_SIZE // 4

    # Reports land next to the bitmap they were derived from, so a
    # multi-hash classify/ tree (e.g. after a rebuild that changes the
    # ROM hash) keeps each pair of reports paired with its bitmap.
    out_dir = bitmap_path.parent
    data_out = out_dir / "data-regions.txt"
    code_out = out_dir / "code-regions.txt"

    data_runs = find_runs(bitmap, total_words, target_bit=0)
    code_runs = find_runs(bitmap, total_words, target_bit=1)

    write_report(data_out, "data", rom_path, rex_path, bitmap_path,
                 aperture, total_words, data_runs)
    write_report(code_out, "code", rom_path, rex_path, bitmap_path,
                 aperture, total_words, code_runs)

    print(f"wrote {data_out} ({data_out.stat().st_size} bytes, "
          f"{len(data_runs)} regions)")
    print(f"wrote {code_out} ({code_out.stat().st_size} bytes, "
          f"{len(code_runs)} regions)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
