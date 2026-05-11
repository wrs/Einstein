@ HVC ABI constants — keep in lockstep with the
@ `HvcImm::GuestTestPrintByte..Debugger` block in
@ src/hvc_imm.rs. The block is anchored at 0x10 there, so adding a
@ variant to the ABI requires bumping every literal here AND
@ updating any test that issues the affected HVC by name.
@
@ Included from `test_runtime.S` (most tests) and from the few
@ self-contained tests that build their own runtime
@ (test_cp15_fault_regs.S, etc.).

    .equ HVC_PRINT_BYTE,        0x10
    .equ HVC_PRINT_HEX,         0x11
    .equ HVC_PASS,              0x12
    .equ HVC_FAIL,              0x13
    .equ HVC_MARK,              0x14
    .equ HVC_GPIO_TRIGGER,      0x15
    .equ HVC_UND,               0x16
    .equ HVC_ALIGN,             0x17
    .equ HVC_SNAPSHOT,          0x18
    .equ HVC_DEBUG_STR,         0x19
    .equ HVC_DEBUGGER,          0x1A
    .equ HVC_INJECT_PEN,        0x1B
