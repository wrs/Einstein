@ tests/test_rom_patches.S
@
@ Verifies the HVC trap handlers installed for the DebugStr / Debugger
@ ROM patches (Einstein-parity for `Emulator/JIT/Generic/
@ TJITGenericROMPatch.cpp:76-102`). Both patches overwrite a Newton
@ UND-table slot with an HVC; on the HVC, the hypervisor logs and
@ returns PC = LR + delta (4 for DebugStr, 8 for Debugger).
@
@ We can't reach the patch sites directly from a guest-test binary
@ (they live at ROM addresses 0x38CE6C / 0x38CE70, outside the test
@ image), so this test exercises the trap-handler side directly: it
@ manually invokes HVC #0x40 / #0x41 with a known LR and verifies
@ execution resumes at LR + 4 / LR + 8.
@
@ The rom-patch-application logic itself (writing the expected bytes
@ to the expected ROM offsets) is covered by Rust-side unit tests in
@ `src/rom_patches.rs`.

#include "../common/test_runtime.S"

    .global test_main
test_main:
    @ The hypervisor's HVC handler for #0x40 / #0x41 reads the guest's
    @ return address from r7 (stashed by the ROM-patch stub via
    @ `MOV r7, LR` before the HVC). This test stashes r7 itself to
    @ simulate what the stub does at the patched call site.

    @ --- DebugStr HVC: PC := r7 + 4 ---
    adr     r7, .Lafter_debugstr
    sub     r7, r7, #4          @ r7 = .Lafter_debugstr - 4
    adr     r0, msg             @ r0 = string pointer for the trap to log
    hvc     #HVC_DEBUG_STR
    hvc_fail 0x90

.Lafter_debugstr:
    mov     r0, #1
    hvc     #HVC_MARK                  @ mark 1: DebugStr HVC resumed at r7+4

    @ --- Debugger HVC: PC := r7 + 8 ---
    adr     r7, .Lafter_debugger
    sub     r7, r7, #8          @ r7 = .Lafter_debugger - 8
    hvc     #HVC_DEBUGGER
    hvc_fail 0x91

.Lafter_debugger:
    mov     r0, #2
    hvc     #HVC_MARK                  @ mark 2: Debugger HVC resumed at r7+8

    hvc_pass
    bx      lr

msg:
    .asciz  "test_rom_patches debug message"
    .balign 4
