@ tests/test_platform_driver.S
@
@ Exercises `peripherals::native_primitives::execute` driver=0x000001
@ (TMainPlatformDriver) via `MCR p10, 0, r5, c0, c0, 0` traps. Subfn
@ semantics mirror Einstein's `TNativePrimitives::ExecutePlatformDriverNative`
@ (`Emulator/TNativePrimitives.cpp:600-1050`).
@
@ Covered:
@   * 0x03 Init                         — r0 = 0
@   * 0x0F PowerOnSystem                — r0 = 0
@   * 0x11 GetPCMCIAPowerSpec(slot=0)   — *r2 = 5,  r0 = 0
@   * 0x11 GetPCMCIAPowerSpec(slot=1)   — *r2 = 7,  r0 = 0
@   * 0x11 GetPCMCIAPowerSpec(slot=2)   — r0 = kError_NotImplemented
@   * 0x14 GetSubsystemPower            — *r2 = 0,  r0 = 0
@   * 0x15 GetNextEvent                 — r0 = 0 (no event)
@   * 0x17 FillGestaltEmulatorInfo      — *r1 = kUP2Version (0x00010003), r0 = 0
@   * 0x18 LockEventQueue               — no-op, no return
@   * 0x19 UnlockEventQueue             — no-op, no return

#include "../common/test_runtime.S"

    .equ OUT_SLOT,        0x04000300
    .equ MCR_P10_R5,      0xEE005A10      @ MCR p10, 0, r5, c0, c0, 0

    .global test_main
test_main:
    @ -------- 0x03 Init -----------------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x0103                    @ (driver=1<<8)|0x03
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_init

    mov     r0, #1
    hvc     #5

    @ -------- 0x0F PowerOnSystem --------------------------------------
    ldr     r0, =0xDEADBEEF
    ldr     r5, =0x010F
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_power_on

    mov     r0, #2
    hvc     #5

    @ -------- 0x11 GetPCMCIAPowerSpec slot 0 --------------------------
    ldr     r4, =OUT_SLOT
    mov     r0, #0xAA
    str     r0, [r4]
    mov     r1, #0                          @ slot 0
    ldr     r2, =OUT_SLOT
    ldr     r5, =0x0111
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_pcmcia0_r0
    ldr     r0, [r4]
    cmp     r0, #5
    bne     .Lfail_pcmcia0_val

    @ -------- 0x11 GetPCMCIAPowerSpec slot 1 --------------------------
    mov     r0, #0xAA
    str     r0, [r4]
    mov     r1, #1                          @ slot 1
    ldr     r2, =OUT_SLOT
    ldr     r5, =0x0111
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_pcmcia1_r0
    ldr     r0, [r4]
    cmp     r0, #7
    bne     .Lfail_pcmcia1_val

    @ -------- 0x11 GetPCMCIAPowerSpec slot 2 (unsupported) -----------
    mov     r1, #2                          @ slot 2 — not implemented
    ldr     r2, =OUT_SLOT
    ldr     r5, =0x0111
    .inst   MCR_P10_R5
    ldr     r1, =0xFFFFD8EB                 @ -10005 = kError_NotImplemented
    cmp     r0, r1
    bne     .Lfail_pcmcia2_err

    mov     r0, #3
    hvc     #5

    @ -------- 0x14 GetSubsystemPower ----------------------------------
    mov     r0, #0xAA
    str     r0, [r4]
    mov     r1, #0                          @ subsystem id (unused by handler)
    ldr     r2, =OUT_SLOT
    ldr     r5, =0x0114
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_subsystem_r0
    ldr     r0, [r4]
    cmp     r0, #0
    bne     .Lfail_subsystem_val

    mov     r0, #4
    hvc     #5

    @ -------- 0x15 GetNextEvent ---------------------------------------
    ldr     r0, =0xDEADBEEF
    mov     r1, #0                          @ event buffer VA (unused)
    ldr     r5, =0x0115
    .inst   MCR_P10_R5
    cmp     r0, #0                          @ no event
    bne     .Lfail_event

    mov     r0, #5
    hvc     #5

    @ -------- 0x17 FillGestaltEmulatorInfo ----------------------------
    mov     r0, #0xAA
    str     r0, [r4]
    ldr     r1, =OUT_SLOT
    ldr     r5, =0x0117
    .inst   MCR_P10_R5
    cmp     r0, #0
    bne     .Lfail_gestalt_r0
    ldr     r0, [r4]
    ldr     r1, =0x00010003                 @ kUP2Version
    cmp     r0, r1
    bne     .Lfail_gestalt_val

    mov     r0, #6
    hvc     #5

    @ -------- 0x18 / 0x19 Lock / UnlockEventQueue ---------------------
    @ Just verify the handler accepts these; no r0 side effect.
    ldr     r5, =0x0118
    .inst   MCR_P10_R5
    ldr     r5, =0x0119
    .inst   MCR_P10_R5

    mov     r0, #7
    hvc     #5

    hvc_pass
    bx      lr

.Lfail_init:
    hvc     #2
    hvc_fail 0x50
.Lfail_power_on:
    hvc     #2
    hvc_fail 0x51
.Lfail_pcmcia0_r0:
    hvc     #2
    hvc_fail 0x52
.Lfail_pcmcia0_val:
    hvc     #2
    hvc_fail 0x53
.Lfail_pcmcia1_r0:
    hvc     #2
    hvc_fail 0x54
.Lfail_pcmcia1_val:
    hvc     #2
    hvc_fail 0x55
.Lfail_pcmcia2_err:
    hvc     #2
    hvc_fail 0x56
.Lfail_subsystem_r0:
    hvc     #2
    hvc_fail 0x57
.Lfail_subsystem_val:
    hvc     #2
    hvc_fail 0x58
.Lfail_event:
    hvc     #2
    hvc_fail 0x59
.Lfail_gestalt_r0:
    hvc     #2
    hvc_fail 0x5A
.Lfail_gestalt_val:
    hvc     #2
    hvc_fail 0x5B
