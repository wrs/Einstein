# baremetal/guest-tests — ARM-guest-side peripheral tests

Tiny AArch32 programs that run *as* the hypervisor's guest and exercise
peripherals via MMIO. Each test replaces the Newton ROM at IPA
`0x0000_0000`, drops some bytes into registers, fires an `HVC` to the
hypervisor to report success or failure, and halts.

These are integration tests: they verify that when the guest does the
same MMIO access patterns the Newton kernel does, the hypervisor
correctly routes them to the right peripheral and returns the right
value — the full EL2 trap path → MMIO dispatcher → peripheral model →
return value into the guest's register.

## HVC protocol (guest → hypervisor)

A test reports its progress via `HVC #imm`; `r0` carries a value if
relevant. The immediates live in `common/hvc_abi.S`, kept in lockstep
with the `HvcImm` enum in `src/hv/hvc_imm.rs` (the guest-test block is
anchored at 0x10 there).

| imm   | name              | meaning                                     | `r0`                 |
|-------|-------------------|---------------------------------------------|----------------------|
| 0x10  | HVC_PRINT_BYTE    | print one ASCII byte                        | char                 |
| 0x11  | HVC_PRINT_HEX     | print a 32-bit hex word                     | value                |
| 0x12  | HVC_PASS          | pass — exit hypervisor OK                   | code                 |
| 0x13  | HVC_FAIL          | fail — exit hypervisor nonzero              | error code / line no |
| 0x14  | HVC_MARK          | mark — print `"mark %08x\n"`                | marker value         |
| 0x15  | HVC_GPIO_TRIGGER  | raise `vic::INT_GPIO` (IRQ-delivery test)   | (ignored)            |
| 0x16  | HVC_UND           | `handle_und` entry (UND-trampoline tag)     | (trampoline state)   |
| 0x17  | HVC_ALIGN         | alignment-fixup entry (EL2 emulates rotate) | (stub state)         |
| 0x18  | HVC_SNAPSHOT      | save the rolling guest-state snapshot       | (ignored)            |
| 0x19  | HVC_DEBUG_STR     | `DebugStr` trap — log guest string          | string address       |
| 0x1A  | HVC_DEBUGGER      | `Debugger` trap — log site                  | (ignored)            |
| 0x1B  | HVC_INJECT_PEN    | inject a pen sample into `host_io::queue`   | packed sample (r1 = ticks) |
| 0x1C  | HVC_REP_RENDER    | render a REP format string into a buffer    | fmt ptr (r1 = out buf) |

The hypervisor in "guest-test mode" prints HVC output to its UART and
halts on HVC_PASS / HVC_FAIL so QEMU exits with a distinguishable
message.

## Test image layout

Each test compiles to a flat binary at load address `0x00000000`. The
first instruction is the guest's reset vector (jumped to by the
hypervisor's `ERET` to EL1 AArch32). Exception vectors 1..6 follow the
Newton convention (they're unused by most tests; typical content is
`b .` so the guest halts on an unexpected trap).

## Building and running

```
scripts/build-tests.sh          # cross-builds all tests/*.S -> *.bin
scripts/run-test.sh test_hello  # builds the hypervisor in guest-test
                                # mode, loads test_hello.bin via
                                # semihosting, runs under QEMU; asserts
                                # HVC_PASS fires.
scripts/run-all.sh              # every MANIFEST test (38); use
                                # --platform fvp for the FVP host.
```

Each test is a single `.S` file with a header comment describing what
it checks. To add a test:

1. Copy `tests/test_hello.S` and edit.
2. Add its name to `tests/MANIFEST`.
3. `scripts/build-tests.sh` picks it up.

Tests link against `common/linker.ld` (text at 0) and are stripped to
flat binary; most pull in `common/test_runtime.S` for the vector table
and the HVC helper macros.

## Where these fit

Guest tests are the behavioural tier of the verification stack: they
run the production trap/dispatch/peripheral code end-to-end, one
handler surface at a time, without needing the Newton ROM. The
structural tiers — `scripts/check-matrix.sh` (feature-combination
builds plus the check-layering / check-rom-addrs lints) and
`scripts/boot-check.sh` (full ROM boot to a known marker) — live in
`baremetal/scripts/`. **Every commit must pass
`guest-tests/scripts/run-all.sh`.**
