@ tests/test_mmio_regs.S
@
@ Spot-checks the Newton hardware-register stubs the Phase A closeout
@ added (or re-validated) in `src/mmio.rs` / `src/peripherals/vic.rs`.
@ These are reads the kernel performs very early (memory probe,
@ platform gestalt). Einstein services each with a fixed value taken
@ from `TMemoryConsts.h` and `TMemory.cpp:868-1147`.
@
@ Covered:
@   * 0x0F000008  PlatformVers         — returns 5 (TPlatformManager::GetVersion)
@   * 0x0F001800  04RAMSize            — returns 0x40400040 (XYXY00XY pattern)
@   * 0x0F001C00  08RAMSize            — returns 0 (second bank absent)
@   * 0x0F110400  HighSpeedClck        — returns 0x90 (kHighSpeedClockVal)

#include "../common/test_runtime.S"

    .equ HW_PLATFORM_VERS,  0x0F000008
    .equ HW_RAM_SIZE_1,     0x0F001800
    .equ HW_RAM_SIZE_2,     0x0F001C00
    .equ HW_HIGH_SPEED,     0x0F110400

    .global test_main
test_main:
    @ --- 0x0F00_0008 PlatformVers == 5 --------------------------------
    ldr     r4, =HW_PLATFORM_VERS
    ldr     r0, [r4]
    cmp     r0, #5
    bne     .Lfail_platform_vers

    mov     r0, #1
    hvc     #HVC_MARK

    @ --- 0x0F00_1800 04RAMSize == 0x40400040 --------------------------
    ldr     r4, =HW_RAM_SIZE_1
    ldr     r0, [r4]
    ldr     r1, =0x40400040
    cmp     r0, r1
    bne     .Lfail_ramsize1

    @ --- 0x0F00_1C00 08RAMSize == 0 -----------------------------------
    ldr     r4, =HW_RAM_SIZE_2
    ldr     r0, [r4]
    cmp     r0, #0
    bne     .Lfail_ramsize2

    mov     r0, #2
    hvc     #HVC_MARK

    @ --- 0x0F11_0400 HighSpeedClck == 0x90 ----------------------------
    ldr     r4, =HW_HIGH_SPEED
    ldr     r0, [r4]
    cmp     r0, #0x90
    bne     .Lfail_high_speed

    mov     r0, #3
    hvc     #HVC_MARK

    hvc_pass
    bx      lr

.Lfail_platform_vers:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x70
.Lfail_ramsize1:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x71
.Lfail_ramsize2:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x72
.Lfail_high_speed:
    hvc     #HVC_PRINT_HEX
    hvc_fail 0x73
