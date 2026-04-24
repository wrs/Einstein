# Phase B boot-stall investigation

Live notes. Update as we learn more; archive to a dated file when
we move past the current stall.

## Currently at — shadow-stub walk-fail at 0xcc80002 in a heap-object write (FVP, 2026-04-24)

Boot now advances past the DDK-UND and translation-fault-forwarding
work (see below) to ~trace 740k, where it halts in the shadow-stub
byte-access emulator:

```
dabt: forwarding to kernel DataAbortHandler — DFSC=0x7 FAR=0x0cc7fcc8 mode=0x17
*** shadow_stub: byte write walk-fail ea=0xcc80002 pc=0x4ed50
```

Context: the user-space function at 0x0004ed10 allocates a 184-byte
object via `__nw__(184)`, receiving `r4 = 0x0cc7ffd4` — a pointer that
spans a page boundary (page `0x0cc7f000` is mapped, but `0x0cc80000`
is not). The constructor then runs `strb r0, [r4, #46]` at PC 0x4ed50,
which should byte-write at VA `0x0cc80002`. Because shadow-stub
patched this STRB as a UDF for BE-32 byte-swap emulation, the
instruction doesn't execute natively — the emulator runs in EL2
Rust, walks stage-1 for the EA, finds it unmapped, and halts.

On unpatched hardware the same access would take a natural DABT,
triggering the kernel's on-demand paging (`TStackManager::
CopyPagesAfterStackCollided` / heap-growth analogue) to map
`0x0cc80000`. Our shadow-stub emulator bypasses that natural fault.

**Dead end explored**: synthesising a DABT from EL2 and ERETing into
an ABT-mode stub that branches to `DataAbortHandler` at VA
`0x0039_3114` was tested and abandoned. The kernel's handler
dispatches on the AArch32 DFSR value
(`mrc p15,0,r1,c5,c0,0 / add pc,pc,r1,LSL #2` at 0x393288..0x393294),
and DFSR32_EL2 writes UNDEF on A53 (see `cp15::write_dfsr32` in
`trap.rs`: "Both MRS and MSR to this register take an EC=0 UNDEFINED
exception at EL2"). Without a valid DFSR, the jump-table lands on the
stale value (0x1 from the last alignment fault) and the handler
reaches its UnhandledException → Reboot path. The stub, constant,
and `return_as_dabt` helper are still in the tree but marked
`#[allow(dead_code)]` for future reference — see
`src/trap.rs::return_as_dabt` for the full analysis.

**Next-session strategies**:
- Pre-fault-in the page from EL2 before emulating the byte access.
  Have handle_sba_udf, when `resolve_addr(access_addr) == None`,
  trigger a harmless natural DABT at that VA (e.g. ERET to an
  AArch32 stub that does a single `ldrb`) and let the kernel's
  own DABT path handle the growth. On return, retry the emulation.
- Alternatively, un-UDF the specific site, ERET to the faulting
  PC so the original STRB runs natively, catch the natural DABT,
  then re-UDF after the kernel's retry succeeds. Requires a hook
  on DABT return (one-shot guest BP at faulting_pc+4).
- Identify why the allocator returned a cross-page pointer in the
  first place — if we could teach the allocator to pre-fault each
  page of its returned range, we'd sidestep the walk-fail entirely.
  Less generally applicable; the DABT mechanism is the Newton
  kernel's standard on-demand-paging path, so we should preserve it.

Reproduce:

```
rm -f /tmp/newton-snapshot-*.bin
cargo build --release --no-default-features --features "platform-fvp-base quiet"
scripts/fvp --timeout=90 target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```
