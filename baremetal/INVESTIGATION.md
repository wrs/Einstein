# Phase B boot-stall investigation

Live notes. Update as we learn more; archive to a dated file when
we move past the current stall.

## Currently at — unrecognised UND at PC=0xb0150 (FVP, 2026-04-24)

With the SBA pre-fault-retry mechanism in place (see "Resolved"
section below), the 0x4ed50 heap-object walk-fail is gone and boot
now reaches a new stall:

```
dabt: forwarding to kernel DataAbortHandler — DFSC=0x7 FAR=0x0cc80002 mode=0x17
dabt: forwarding to kernel DataAbortHandler — DFSC=0x7 FAR=0x0cc81000 mode=0x17
und: DebuggerUND @PC=0x393898 msg="<bad utf-8>" (resume at PC=0x3938c4)
*** unrecognised UND: insn=0xeb6cc3e1 at PC=0xb0150 SPSR_und=0x1db
    (extend handle_und in trap.rs to handle this opcode)
```

`SPSR_und = 0x1DB` → pre-UND mode was UND (nested UND — A=1, I=1,
F=1, T=0, flags=0). The insn `0xeb6cc3e1` decodes as `BL <offset>`
which should not UND-fault, so either (a) the CPU is running off
into data at an unexpected PC, (b) the kernel is in a Debugger-UND
handler path and this is an expected exit that we need to recognise,
or (c) the prior DebuggerUND at 0x393898 left the guest in a state
where PC=0xb0150 is wrong. The msg="<bad utf-8>" on the DebuggerUND
line suggests the scanned string is not a real UTF-8 string — maybe
the msg_start/msg_end computation needs to be BE-32-aware. Start
there: dump bytes around 0x39389c and see if the BE-32 XOR gives
readable ASCII.

Reproduce:

```
rm -f /tmp/newton-snapshot-*.bin
cargo build --release --no-default-features --features "platform-fvp-base quiet"
scripts/fvp --timeout=120 target/aarch64-unknown-none-softfloat/release/newton-hypervisor
```

## Resolved — shadow-stub walk-fail at 0xcc80002 (2026-04-24)

The heap-object STRB at PC=0x4ed50 writing to an unmapped cross-page
VA is now handled via the SBA pre-fault retry round-trip:

1. `handle_sba_udf` in `src/shadow_stub.rs` detects the failed stage-1
   resolve BEFORE reaching `dispatch_*_write` and stashes emulator
   state (ctx snapshot, faulting_pc, spsr_und, idx) into Rust statics.
2. Overwrites `ctx.x[0]` with the EA and ERETs (SPSR_EL2 left at UND)
   into a 3-word ROM-resident probe stub at `SBA_PREFAULT_STUB_VA =
   0x00FF_FF60`: `LDRB r0, [r0]; HVC #SBA_RETRY_TAG; B .`.
3. The probe's LDRB takes a natural DABT for the unmapped page. The
   existing DABT trampoline + `handle_diag` forward path invokes the
   kernel's own `DataAbortHandler` at VA `0x0039_3114`. The kernel's
   demand-pager grows the heap and retries the LDRB via
   `subs pc, lr, #8`.
4. Probe succeeds, falls through to the HVC, and
   `handle_sba_retry` restores the stashed ctx + re-runs the emulator
   body with the same idx. This time `resolve_addr(access_addr)`
   succeeds and the byte write lands.

The dead-end `return_as_dabt` synthesis path (previously marked
`#[allow(dead_code)]` after the A53 DFSR32_EL2 UNDEF investigation)
has been removed; so have the `DABT_INJECT_*` constants and the
dead ROM-tail stub that used to sit at `0x00FF_FF60`.

**Side-effect fix — demand-paged RAM code pages**: the stage-2 RAM
mapping was refined from 2 MiB blocks to 4 KiB pages with a per-page
`RW + XN ↔ RO + ¬XN` state machine. `handle_instruction_abort` now
scans one 4 KiB page (not 2 MiB), and a new stage-2 RO permission
fault branch in `handle_data_abort` re-arms a frozen code page as
`RW + XN` when the kernel's demand-pager overwrites it. On the next
fetch the page is re-scanned. See `subtest_21_ram_reload` in
`test_shadow_stub.S` for the regression test.

**Inline-stub fast path (deferred to v2)**: The plan's per-site
11-word inline stub is implemented in `src/shadow_stub.rs` behind
a `const INLINE_STUBS_ENABLED: bool = false` gate. Initial FVP
testing showed the stack-push (`STR scratch_ea, [SP, #-4]!`)
writes into ROM when SP_<mode> is still at its reset value pre-
SetUpStacks. v2 strategies: (1) split inline-eligibility by
"known-post-SetUpStacks" PC range; (2) move scratch save to a
TPIDR-style mode-agnostic slot; (3) validate SP before the push
via a conditional skip. For now every site uses the UDF + pre-
fault retry path, which is correctness-equivalent at a 3-trap
round-trip overhead on walk-fails (0-trap otherwise).
