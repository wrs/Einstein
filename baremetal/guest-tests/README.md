# baremetal/guest-tests — ARM-guest-side peripheral tests

Tiny AArch32 programs that run *as* the hypervisor's guest and exercise
peripherals via MMIO. Each test replaces the Newton ROM at IPA
`0x0000_0000`, drops some bytes into registers, fires an `HVC` to the
hypervisor to report success or failure, and halts.

These are integration tests: they verify that when the guest does the
same MMIO access patterns the Newton kernel does, the hypervisor
correctly routes them to the right peripheral and returns the right
value. They complement the C++ host tests in `baremetal/cxx-core/tests`
(which test the peripheral class in isolation) — we run both to
distinguish "the peripheral is broken" from "the FFI / MMIO dispatch is
broken."

## HVC protocol (guest → hypervisor)

A test reports its progress via `HVC #imm`; `r0` carries a value if
relevant.

| imm   | meaning                           | `r0`                 |
|-------|-----------------------------------|----------------------|
| 0x01  | print one ASCII byte              | char                 |
| 0x02  | print a 32-bit hex word           | value                |
| 0x03  | pass — exit hypervisor OK         | (ignored)            |
| 0x04  | fail — exit hypervisor nonzero    | error code / line no |
| 0x05  | mark — print `"mark %08x\n"`      | marker value         |

The hypervisor in "guest-test mode" prints HVC output to its UART and
halts on 0x03 / 0x04 so QEMU exits with a distinguishable message.

## Test image layout

Each test compiles to a flat binary at load address `0x00000000`. The
first instruction is the guest's reset vector (jumped to by the
hypervisor's `ERET` to EL1 AArch32). Exception vectors 1..6 follow the
Newton convention (they're unused by most tests; typical content is
`b .` so the guest halts on an unexpected trap).

## Building and running

```
scripts/build-tests.sh          # cross-builds all tests/*.S -> *.bin
scripts/run-test.sh test_hello  # embeds test_hello.bin into a hypervisor
                                # build and runs under QEMU; asserts HVC
                                # 0x03 fires.
```

Each test is a single `.S` file with a header comment describing what
it checks. To add a test:

1. Copy `tests/test_hello.S` and edit.
2. Add its name to `tests/MANIFEST`.
3. `scripts/build-tests.sh` picks it up.

No linker script today — tests are built with `-Ttext=0 -N` and
stripped to flat binary.

## Why both tiers?

- Host tests (`baremetal/cxx-core/tests`) run the peripheral C++ code
  in a friendly x86-64 environment. Fast feedback on shim correctness,
  C ABI contracts, and algorithmic behaviour.
- Guest tests (this directory) validate that when everything is
  assembled on bare metal — EL2 trap path → Rust MMIO dispatcher →
  C-ABI shim → C++ peripheral → shim → return value to guest — the
  right value shows up in the guest's register.

If the host test passes and the guest test fails, the bug is somewhere
in the EL2 / dispatch / FFI layer. If both fail, it's in the peripheral
C++.
