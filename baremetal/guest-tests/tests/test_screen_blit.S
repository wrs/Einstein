@ tests/test_screen_blit.S
@
@ Verifies the Screen/Blit native primitive copies bitmap bytes
@ from a PixelMap in guest RAM into the framebuffer region.
@
@ Setup (all in guest RAM):
@   pixmap @ 0x04020000 = {
@     addy = 0x04021000,   // pointer to bitmap data
@     rowBytes = 4,
@     bounds = {0, 0, 2, 32}  // 2 rows x 32 pixels (1 bpp)
@     flags = 0,
@     table = 0,
@   }
@   bitmap data @ 0x04021000: 8 bytes { 0xDE 0xAD 0xBE 0xEF 0xCA 0xFE 0xBA 0xBE }
@   srcRect @ 0x04030000 = {0, 0, 2, 32}
@   dstRect @ 0x04030010 = {0, 0, 2, 32}
@
@ Issue the blit via `.inst 0xEE003A10` (MCR p10,0,r3,c0,c0,0) with
@ r0 = 0x0000_0407 (driver=4 screen, subfn=7 blit), r1 = pixmap VA,
@ r2 = srcRect VA, r3 = dstRect VA (the r3-as-MCR-source slot holds
@ the native code so is repurposed — we move it to r3 right before
@ the MCR and use a different register for the native call code).
@
@ Actually, the hypervisor reads ctx.x[Rd] where Rd is the MCR's
@ source register. The encoding 0xEE003A10 has Rd=3, so the native
@ code must be in r3 at the MCR. That clashes with Einstein's
@ convention of r1/r2/r3 being the blit arguments. Resolve by using
@ a different MCR encoding with a higher Rd (e.g. r5 via 0xEE005A10):
@   0xEE005A10: MCR p10, 0, r5, c0, c0, 0
@ so the native code lives in r5 and r1/r2/r3 keep their blit meaning.
@
@ After the call, read FB @ 0x0E000000..0x0E000008 and verify it
@ matches the planted bytes.

#include "common/test_runtime.S"

    .equ PIXMAP_VA,      0x04020000
    .equ PIXMAP_DATA_VA, 0x04021000
    .equ SRC_RECT_VA,    0x04030000
    .equ DST_RECT_VA,    0x04030010
    .equ FB_VA,          0x0E000000
    .equ BLIT_CODE,      0x0407       @ driver=4, subfn=7

    .section .text, "ax"
    .global test_main
test_main:
    @ ---- Build the PixelMap struct ----
    ldr     r6, =PIXMAP_VA

    ldr     r0, =PIXMAP_DATA_VA
    str     r0, [r6, #0x00]          @ addy

    mov     r0, #4
    str     r0, [r6, #0x04]          @ rowBytes

    @ bounds = {top=0, left=0, bottom=2, right=32}.
    @ Word 0 = (top<<16) | left = 0
    mov     r0, #0
    str     r0, [r6, #0x08]
    @ Word 1 = (bottom<<16) | right = (2<<16)|32
    mov     r0, #2
    lsl     r0, r0, #16
    orr     r0, r0, #32
    str     r0, [r6, #0x0C]
    mov     r0, #0
    str     r0, [r6, #0x10]          @ flags
    str     r0, [r6, #0x14]          @ table

    @ ---- Plant bitmap bytes (8 bytes, 2 rows * 4 bytes) ----
    ldr     r6, =PIXMAP_DATA_VA
    ldr     r0, =0xEFBEADDE          @ LE word = DE AD BE EF
    str     r0, [r6, #0]
    ldr     r0, =0xBEBAFECA          @ LE word = CA FE BA BE
    str     r0, [r6, #4]

    @ ---- Build src + dst rects (same shape, top-left dst) ----
    ldr     r6, =SRC_RECT_VA
    mov     r0, #0
    str     r0, [r6, #0]
    mov     r0, #2
    lsl     r0, r0, #16
    orr     r0, r0, #32
    str     r0, [r6, #4]

    ldr     r6, =DST_RECT_VA
    mov     r0, #0
    str     r0, [r6, #0]
    mov     r0, #2
    lsl     r0, r0, #16
    orr     r0, r0, #32
    str     r0, [r6, #4]

    @ ---- Issue the blit ----
    ldr     r1, =PIXMAP_VA
    ldr     r2, =SRC_RECT_VA
    ldr     r3, =DST_RECT_VA
    ldr     r5, =BLIT_CODE           @ native call code in r5
    .inst   0xEE005A10               @ MCR p10, 0, r5, c0, c0, 0

    @ The handler writes r0 = 0 on success.
    cmp     r0, #0
    bne     .Lfail_not_zero

    mov     r0, #1
    hvc     #5                       @ mark 1: blit returned success

    @ ---- Verify FB bytes ----
    ldr     r6, =FB_VA
    ldr     r0, [r6, #0]
    ldr     r1, =0xEFBEADDE
    cmp     r0, r1
    bne     .Lfail_fb0

    ldr     r0, [r6, #4]
    ldr     r1, =0xBEBAFECA
    cmp     r0, r1
    bne     .Lfail_fb1

    mov     r0, #2
    hvc     #5                       @ mark 2: FB has expected bytes

    hvc     #3                       @ PASS
    bx      lr

.Lfail_not_zero:
    hvc     #2                       @ print observed r0
    mov     r0, #0xD0
    hvc     #4
.Lfail_fb0:
    hvc     #2                       @ print observed value
    mov     r0, #0xD1
    hvc     #4
.Lfail_fb1:
    hvc     #2
    mov     r0, #0xD2
    hvc     #4
