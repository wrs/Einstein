#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
"""Walk the classifier's reach.bitmap and emit a hex dump of every
contiguous data region (consecutive 32-bit words with reach bit = 0).

The output is in BE numerical view (matching what a CPSR.E=1 LDR would
return on the guest), 8 words per line, with ASCII gloss and a heuristic
annotation when a word looks like an in-ROM function pointer.

Pure-zero runs collapse to a single "ZERO FILL" summary line. Output
goes to baremetal/data-regions.txt by default; pass --out to override.
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


def is_data_word(bitmap: bytes, idx: int) -> bool:
    """True iff word index `idx` is *not* marked as reachable code."""
    byte = idx >> 3
    bit = idx & 7
    if byte >= len(bitmap):
        return True
    return ((bitmap[byte] >> bit) & 1) == 0


def find_data_runs(bitmap: bytes, total_words: int) -> list[tuple[int, int]]:
    """Return [(start_word_idx, end_word_idx_exclusive), ...] for every
    contiguous run of data words. Skips runs that are zero-length."""
    runs: list[tuple[int, int]] = []
    i = 0
    while i < total_words:
        if is_data_word(bitmap, i):
            j = i
            while j < total_words and is_data_word(bitmap, j):
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


def dump_region(
    aperture: bytes,
    start_word: int,
    end_word: int,
    f,
) -> None:
    start_pa = start_word * 4
    end_pa = end_word * 4
    word_count = end_word - start_word
    byte_count = word_count * 4

    # Read all words for this region (BE numerical).
    words = list(struct.unpack(f">{word_count}I", aperture[start_pa:end_pa]))

    print(f"{byte_count} bytes", file=f)
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


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rom", type=Path, default=Path("roms/newton.rom"))
    ap.add_argument("--rex", type=Path, default=Path("../_Data_/Einstein.rex"))
    ap.add_argument("--classify-dir", type=Path, default=Path("classify"))
    ap.add_argument("--out", type=Path, default=Path("data-regions.txt"))
    args = ap.parse_args()

    here = Path(__file__).resolve().parent.parent
    rom_path = (here / args.rom).resolve()
    rex_path = (here / args.rex).resolve()
    classify_dir = (here / args.classify_dir).resolve()
    out_path = (here / args.out).resolve()

    bitmap_path = find_bitmap(classify_dir)
    bitmap = bitmap_path.read_bytes()
    aperture = load_rom_aperture(rom_path, rex_path)
    total_words = ROM_SIZE // 4
    runs = find_data_runs(bitmap, total_words)

    with out_path.open("w") as f:
        print(f"# data-regions report", file=f)
        print(f"# rom:     {rom_path}", file=f)
        print(f"# rex:     {rex_path}", file=f)
        print(f"# bitmap:  {bitmap_path}", file=f)
        print(
            f"# {len(runs)} contiguous data regions across "
            f"{total_words} words ({total_words*4} bytes)",
            file=f,
        )
        total_data_words = sum(e - s for s, e in runs)
        print(
            f"# {total_data_words} data words "
            f"({total_data_words*4} bytes, "
            f"{100.0*total_data_words/total_words:.1f}% of aperture)",
            file=f,
        )
        print(file=f)
        for start_word, end_word in runs:
            dump_region(aperture, start_word, end_word, f)

    print(f"wrote {out_path} ({out_path.stat().st_size} bytes, {len(runs)} regions)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
