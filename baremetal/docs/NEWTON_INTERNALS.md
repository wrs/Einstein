# Newton 2.x internals cheatsheet

Short-form notes on 717006-ROM conventions that recur when debugging
Phase B. Check here before reverse-engineering from bare disassembly.

## Calling convention — APCS, not AAPCS

The 717006 ROM (1995–1997) uses ARM's original APCS-32, not the later
AAPCS. Every function prologue looks like:

```arm
mov  ip, sp
push {r0, r1, r2, r3}              ; save arg regs (APCS tradition)
push {r4..r9, sl, fp, ip, lr, pc}  ; callee-saved + hidden frame regs
sub  fp, ip, #<N>                  ; fp = saved-frame anchor
```

Differences from AAPCS that matter when reading ROM disassembly:

- **`pc` is pushed into the stack frame.** APCS pushes `pc` via
  `push {..., pc}`; used by debuggers to identify frames. AAPCS never
  does this.
- **`fp = ip − <N>`** where `<N> = 4 + 4×(#saved-regs-after-ip)`. fp
  points just above the saved PC slot. Offsets from fp:
  - `[fp+0]` = saved pc
  - `[fp+4]` = first saved-arg-reg (r0)
  - `[fp+8]` = r1, `[fp+12]` = r2, `[fp+16]` = r3
  - `[fp+20]` = first **stack** arg (arg5), etc.
  (arg4, the first stack arg in some modern docs, is at `[fp+20]`, NOT
  `[fp+8]`.)
- **Stack alignment:** APCS only requires 4-byte at function
  boundaries; don't assume AAPCS 8-byte alignment.
- **Soft-float only.** No VFP regs used for float args.
- **Return values:** r0 (32-bit) or r0/r1 (64-bit), same as AAPCS.

Name mangling is **MPW-style** (not Itanium C++ ABI). Example:
`Init__5TTaskFPFPvUlT2_vUlPvN32P12TEnvironment` decodes as:
class `TTask`, method `Init`, args
`PF(Pv,Ul,T2)_v, Ul, Pv, N32 (=3×Ul), P12TEnvironment`.
`N<n><k>` = n copies of the k-th previous arg. `T<k>` = same as k-th
arg.

## Object / vtable layout — two-level dispatch

Newton's C++ ABI is NOT a standard vtable. Dispatch is two indirections:

```
object @ this
  +4           → ClassInfo* (NOT a direct vtable)
ClassInfo @ *(this+4)
  +8           → method-array base
method-array @ that
  +<offset>    → real method PC
```

Reads as 3 loads + one `add pc`:

```arm
ldr  r0, [r0, #4]          ; r0 = ClassInfo
ldr  r12, [r0, #8]          ; r12 = method-array base
add  pc, r12, #<offset>    ; jump to method
```

Method offsets are NOT `sizeof(ptr)*index` — there's a header area
(hidden `new` / `delete` slots). Empirically `PauseSystem` is the 11th
member method of `TPlatformDriver` but lands at `+0x38` (not `+0x2C`),
so ~3 words of class-header slots precede user methods.

If faking a global (e.g. `gPlatformDriver`) to bypass a stall:
(a) the object needs `this+4` → valid ClassInfo,
(b) the ClassInfo needs `+8` → method-array,
(c) method offsets must match ROM source order (compute from
`_Data_/symbols.txt` starting at `Delete__<class>`).

Note: `MainConstructor` / `TheMain` / `GetSizeOf` are NOT virtual —
they're part of ClassInfo. Virtual-dispatch targets live in the
method-array reached via ClassInfo+8.

## ROM patch table at VA 0x01A00000..0x01C20880 — post-ship patch mechanism

Not a class registry, dispatch table, or symbol table. It's a **thunk
table for post-shipping patches**: the ROM shipped non-reprogrammable,
so Apple introduced this indirection layer.

- Default entry: `B <real_rom_function>` — still in the ROM image,
  aliased into the VA range via stage-1 small pages. Calling the
  patch-table VA jumps straight to the ROM function (1-cycle
  indirection).
- Patched entry: the one 4KB VA page carrying that slot is remapped
  from ROM to a RAM copy, and the RAM copy's word is rewritten to
  `B <ram_patch_function>`. Patch bodies live in a separate "patch
  RAM" area.

### Layout

Physical (patchtable only; jumptable-proper and page-tables live
elsewhere and are not VA-mapped into this range):

  Patchtable  phys 0x02000 - 0x1285C   (529 slots × 0x80 bytes = 32 × B-thunk per slot)

Virtual:

  17 *buckets* of 0x20000 bytes at `0x01A00000 + B * 0x20000`,
  `B ∈ 0..16`. Each bucket contains 32 VA 4KB pages, **all aliased
  to the same** physical 4KB page at `0x2000 + B * 0x1000`.

  Within a bucket, slot `P ∈ 0..31` is a 0x80-byte (32-entry)
  region at `VA = bucket_start + P*0x1000 + P*0x80`, entries at
  `phys = bucket_phys + P*0x80`. The intra-page offset shift
  (`P*0x80`) puts each slot on its own VA 4KB page, so remapping
  only that page to RAM patches only its 32 thunks while leaving
  the other 31 slots of the same physical page untouched — that's
  the whole point of the aliasing scheme.

  For the Ghidra setup, see `/Users/walter/Projects/newton/ghidra/`
  scripts: `RebuildNewtonJumpTableMapping.py` creates the 544
  byte-mapped blocks, and `SetNewtonJumpTableThunks.py` converts
  every slot entry to a Ghidra thunk so the decompiler sees through
  to the ROM target.

Implications:

- A BL to `0x01Axxxxx` / `0x01Bxxxxx` / `0x01Cxxxxx` is calling the
  *patchable* version of the default ROM target.
- ClassInfo method slots using the patch-table VA are opting into
  patchability.
- The tracer DOES see through these thunks — the target is an
  ordinary ROM function, so its first-word tracer trampoline fires
  normally. What the tracer does NOT see is the patch-table thunk
  itself (not in `scripts/classify-out/code-symbols.txt`).

For real class/task registration hunts, search
`TPrivatePackageIterator`, `FindHighROMProtocol`,
`TClassInfoRegistryImpl::Register`, etc. — the actual registration
path. The patch table is not it.

## DDK headers — authoritative struct / API definitions

`/Users/walter/Projects/newton/ghidra/DDKIncludes/` (also mirrored at
`/Users/walter/Downloads/Lantern DDK 1.0 Beta/Source/DDKIncludes/`).

Check here FIRST before reversing a struct from disassembly.

| Topic | Header |
| --- | --- |
| Kernel types / IDs | `OS600/KernelTypes.h` |
| Task / scheduler   | `OS600/UserTasks.h` (TUTask, TUTaskWorld, STaskSwitchedGlobals) |
| Domains            | `OS600/UserDomain.h` (TUDomain) |
| Environments       | `OS600/UserGlobals.h` |
| Ports / Messages   | `OS600/UserPorts.h`, `UserMonitor.h`, `UserSharedMem.h` |
| Shared mem / phys  | `OS600/UserPhys.h`, `UserSemaphore.h`, `UserSharedMem.h` |
| Name server        | `OS600/NameServer.h` |
| Objects            | `OS600/UserObjects.h` |
| Errors             | `OS600/OSErrors.h`, `NewtErrors.h`, `NewtonExceptions.h` |
| Memory             | `NewtonMemory.h` |
| ROM-extension layout | `OS600/ROMExtension.h` (matches our `guest_mem.rs` REx parser) |
| Protocols          | `OS600/Protocols.h` |
| Config             | `OS600/ConfigOS600.h`, `NewtConfig.h`, `Newton.h` |

Public `TUTask` / `TUDomain` classes in these headers match what ROM
exposes; internal `TTask` etc. may not be here but the public wrappers
constrain the surface.

Nearby: `/Users/walter/Projects/newton/ghidra/` has Ghidra projects
for both the 717006 ROM and NewtonOS. `mmu.txt` describes the
post-ship patch-table aliasing (17 buckets × 32 aliased VA pages);
`RebuildNewtonJumpTableMapping.py`, `SetNewtonJumpTableThunks.py`,
and `ReclassifyRomDataRanges.py` are the import helpers.
