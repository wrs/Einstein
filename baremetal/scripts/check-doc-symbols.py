#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
"""Check that code references in the docs still resolve.

Markdown prose cites our own code as `module::item` or
`path/file.rs::item`. Those citations rot silently: a rename sweep that
rewrites the namespace in prose without checking the item moved with it
leaves a reference that looks freshly maintained and is wrong. Worse, it
can point at a real module that happens not to define the item, which
reads as more authoritative than a dangling reference to a deleted one.

Two checks, both over inline code spans only (unquoted prose is too
noisy to parse):

  UNRESOLVED   the item is defined nowhere in the tree.
  MISATTRIBUTED  the item exists, but not in the module the doc names.

Newton- and Einstein-side C++ symbols (TScheduler::Schedule) are
CamelCase and legitimately absent from this tree, so only snake_case /
SCREAMING_CASE items under a snake_case module are audited.

A doc that deliberately names something not yet written can say so with
`<!-- doc-symbols: proposed -->` on that line.

Exit status is 1 if anything failed to resolve, so this can gate a
commit the way check-layering.sh and check-rom-addrs.sh do.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Put this on a line whose `module::item` names something intentionally
# not yet implemented; the line is then skipped.
PROPOSED_MARKER = "<!-- doc-symbols: proposed -->"
SRC_DIRS = ["src", "tools", "newton-objects"]
SKIP_DIRS = {"target", "vendor", ".git", ".jj", "build"}

# Namespaces that are not modules in this tree: primitives (`u32::MAX`),
# the standard library, and build-script directives (`cargo::rustc-*`).
NOT_OUR_MODULES = {
    "u8", "u16", "u32", "u64", "u128", "usize",
    "i8", "i16", "i32", "i64", "i128", "isize",
    "f32", "f64", "bool", "char", "str",
    "core", "std", "alloc", "crate", "self", "super", "Self",
    "cargo", "p15", "p14", "p10", "p11",
}

# A definition of `sym`. Covers `static mut NAME` and `pub(crate) fn
# NAME`, both of which a naive keyword-then-name pattern misses.
def def_re(sym: str) -> re.Pattern:
    s = re.escape(sym)
    vis = r"(?:pub(?:\([^)]*\))?\s+)?"
    return re.compile(
        rf"(?:^|\s){vis}(?:unsafe\s+|extern\s+\"[^\"]*\"\s+|async\s+)*"
        rf"(?:fn|struct|enum|trait|type|union|mod)\s+{s}\b"
        rf"|(?:^|\s){vis}(?:const|static)\s+(?:mut\s+)?{s}\b"
        rf"|macro_rules!\s+{s}\b",
        re.M,
    )


def block_at(text: str, open_brace: int) -> str:
    """Body of the {...} starting at `open_brace`, by brace counting."""
    depth = 0
    for i in range(open_brace, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return text[open_brace : i + 1]
    return text[open_brace:]


def source_files() -> list[Path]:
    out = []
    for d in SRC_DIRS:
        base = ROOT / d
        if not base.exists():
            continue
        for p in base.rglob("*.rs"):
            if not any(part in SKIP_DIRS for part in p.parts):
                out.append(p)
    return sorted(out)


def reexport_index(blobs: dict[Path, str]) -> dict[str, str]:
    """module name -> text of its `pub use ...;` statements.

    A module can publish an item it does not define, by name
    (`pub use imp::{FOO, BAR};` in rom_ver/mod.rs). Citing it as
    `rom_ver::FOO` is correct, so the module owns it for our purposes.
    """
    out: dict[str, str] = {}
    stmt = re.compile(r"^\s*pub\s+use\s[^;]*;", re.M)
    for p, text in blobs.items():
        name = p.parent.name if p.stem == "mod" else p.stem
        out[name] = out.get(name, "") + "\n".join(stmt.findall(text))
    return out


def module_index(blobs: dict[Path, str]) -> dict[str, list[tuple[Path, str]]]:
    """module name -> [(defining file, text to search)].

    A module is a file (`foo.rs`, or `foo/mod.rs` naming `foo`) or an
    inline `mod foo { ... }` block. Inline modules are real and cited in
    the docs, so a file-only index reports them as "no such module".
    """
    mods: dict[str, list[tuple[Path, str]]] = {}
    inline = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+([a-z][a-z0-9_]*)\s*\{", re.M)
    glob_reexport = re.compile(r"^\s*pub\s+use\s+[\w:]*\*\s*;", re.M)
    for p, text in blobs.items():
        name = p.parent.name if p.stem == "mod" else p.stem
        mods.setdefault(name, []).append((p, text))
        for m in inline.finditer(text):
            body = block_at(text, m.end() - 1)
            mods.setdefault(m.group(1), []).append((p, body))
        # A module that glob-re-exports (`pub use imp::*;`, where `imp`
        # is a cfg-selected backend) publishes its siblings' items under
        # its own path, so `platform::enable_bcm2835_irq` is a valid
        # citation even though the fn is defined in platform/raspi3b.rs.
        if p.stem == "mod" and glob_reexport.search(text):
            for sib in p.parent.rglob("*.rs"):
                if sib != p:
                    mods[name].append((sib, blobs.get(sib, sib.read_text(errors="replace"))))
    return mods


def main() -> int:
    srcs = source_files()
    blobs = {p: p.read_text(encoding="utf-8", errors="replace") for p in srcs}
    allsrc = "\n".join(blobs.values())
    mods = module_index(blobs)
    reexports = reexport_index(blobs)

    docs = sorted(p for p in ROOT.rglob("*.md") if not any(x in SKIP_DIRS for x in p.parts))

    sym_re = re.compile(r"`([A-Za-z_][\w/.]*(?:\.rs)?)::([A-Za-z_]\w*)")
    path_re = re.compile(r"`((?:src|tools|scripts|probe|guest-tests|newton-objects)/[\w./-]+)`")

    unresolved: list[str] = []
    misattributed: list[str] = []
    badpaths: list[str] = []

    for doc in docs:
        rel = doc.relative_to(ROOT)
        for lineno, line in enumerate(doc.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
            # Docs legitimately name things that do not exist yet, when
            # proposing them ("a `host_dma::abort_channel` helper would
            # be worth adding"). Marking the line says that is deliberate
            # rather than rotted.
            if PROPOSED_MARKER in line:
                continue
            for owner, sym in sym_re.findall(line):
                if owner in NOT_OUR_MODULES or sym == "mod":
                    continue
                # Only our own Rust naming shape.
                if not (owner.endswith(".rs") or re.fullmatch(r"[a-z][a-z0-9_]*", owner)):
                    continue
                if not re.fullmatch(r"[a-z][a-z0-9_]*|[A-Z][A-Z0-9_]*", sym):
                    continue

                pat = def_re(sym)
                if not pat.search(allsrc):
                    unresolved.append(f"  {rel}:{lineno}  {owner}::{sym}")
                    continue

                name = owner[:-3] if owner.endswith(".rs") else owner
                parts = name.split("/")
                name = parts[-1]
                # `src/hv/trap/mod.rs::foo` names the `trap` module, not
                # a module called `mod`.
                if name == "mod" and len(parts) >= 2:
                    name = parts[-2]
                owners = mods.get(name, [])
                if not owners:
                    misattributed.append(f"  {rel}:{lineno}  {owner}::{sym}  -- no module named '{name}'")
                elif re.search(rf"\b{re.escape(sym)}\b", reexports.get(name, "")):
                    pass  # published by name from another module
                elif not any(pat.search(t) for _, t in owners):
                    where = [str(p.relative_to(ROOT)) for p in srcs if pat.search(blobs[p])]
                    misattributed.append(
                        f"  {rel}:{lineno}  {owner}::{sym}  -- defined in {', '.join(where[:3])}"
                    )

            for path in path_re.findall(line):
                # Doc-relative first (guest-tests/README.md says
                # `scripts/build-tests.sh`, meaning its own sibling).
                if not (doc.parent / path).exists() and not (ROOT / path).exists():
                    badpaths.append(f"  {rel}:{lineno}  {path}")

    for title, items in (
        ("UNRESOLVED — no definition anywhere in the tree", unresolved),
        ("MISATTRIBUTED — exists, but not in the module named", misattributed),
        ("MISSING PATHS", badpaths),
    ):
        if items:
            print(f"{title}:")
            print("\n".join(items))
            print()

    total = len(unresolved) + len(misattributed) + len(badpaths)
    print(f"check-doc-symbols: {len(docs)} docs, {len(srcs)} source files, {total} problem(s)")
    return 1 if total else 0


if __name__ == "__main__":
    sys.exit(main())
