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

## ROM jump-table at VA 0x01A00000..0x01C20000 — post-ship patch mechanism

Not a class registry, dispatch table, or symbol table. It's a **thunk
table for post-shipping patches**: the ROM shipped non-reprogrammable,
so Apple introduced this indirection layer.

- Default entry: `B <real_rom_function>` — still in the ROM image,
  mapped into the VA range via stage-1 small pages. Calling the
  jump-table VA jumps straight to the ROM function (1-cycle
  indirection).
- Patched entry: the small page is remapped from ROM to a RAM copy,
  and the RAM copy's word is rewritten to `B <ram_patch_function>`.
  Patch bodies live in a separate "patch RAM" area.

Implications:

- A BL to `0x01Axxxxx` / `0x01Bxxxxx` / `0x01Cxxxxx` is calling the
  *patchable* version of the default ROM target.
- ClassInfo method slots using the jump-table VA are opting into
  patchability.
- The tracer DOES see through these thunks — the target is an
  ordinary ROM function, so its first-word tracer trampoline fires
  normally. What the tracer does NOT see is the jump-table thunk
  itself (not in `scripts/classify-out/code-symbols.txt`).

For real class/task registration hunts, search
`TPrivatePackageIterator`, `FindHighROMProtocol`,
`TClassInfoRegistryImpl::Register`, etc. — the actual registration
path. The jump table is not it.

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

Nearby: `/Users/walter/Projects/newton/ghidra/` has Ghidra projects for
both the 717006 ROM and NewtonOS plus `NOTES.md` describing the
MMU-mapped jump-table layout (`01A00000 → 00002000 ×32`).
