@ tests/test_screen_blit.S
@
@ Verifies the Screen/Blit native primitive copies bitmap bytes
@ from a PixelMap in guest RAM into the framebuffer region.
@
@ Setup (all in guest RAM):
@   pixmap @ 0x04020000 = {
@     addy = 0x04021000,                     // pointer to bitmap data
@     rowBytes_packed = (4 << 16) = 0x40000, // actual rowBytes is the
@                                            // HIGH 16 bits per
@                                            // TScreenManager::Blit
@                                            // (`srcRowBytes >> 16`).
@     bounds = {0, 0, 2, 16}  // 2 rows x 16 pixels (2 bpp = 32 bits = 4 bytes)
@     flags = 0,
@     table = 0,
@   }
@   bitmap data @ 0x04021000: 8 bytes { 0xDE 0xAD 0xBE 0xEF 0xCA 0xFE 0xBA 0xBE }
@   srcRect @ 0x04030000 = {0, 0, 2, 16}
@   dstRect @ 0x04030010 = {0, 0, 2, 16}
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
@ After the call, read FB. The hypervisor's blit:
@   - carries source bytes verbatim (no inversion),
@   - lays rows out in FB at stride SCREEN_WIDTH*2/8 = 80 bytes/row
@     (NOT the source rowBytes).
@ So row 0 (4 src bytes) lands at FB[0..4], row 1 lands at FB[80..84],
@ with each byte preserved unchanged.
@
@ Two further blits cover the handler's other paths:
@   B — mode 1 ("darken only" ink merge) into a dst rect at left=2,
@       exercising the slow byte-merge path: per pixel the result is
@       max(src, dst), and pixels outside the rect within the touched
@       dst bytes must survive.
@   C — mode 0, byte-aligned, with a source row that straddles a
@       4 KiB page boundary, exercising the fast path's per-page span
@       split.
@
@ The orientation section then covers SetFeature(4)/GetFeature(4),
@ the GetScreenInfo geometry swap, the rotated blit path (D), and
@ the pen-sample rotation into screen space.

#include "common/test_runtime.S"

    .equ PIXMAP_VA,       0x04020000
    .equ PIXMAP_DATA_VA,  0x04021000
    .equ PIXMAP_DATA2_VA, 0x04021100  @ mode-1 merge source (blit B)
    .equ PIXMAP_DATA3_VA, 0x04021FF8  @ row crosses the 4 KiB boundary
                                      @ at 0x04022000 (blit C)
    .equ SRC_RECT_VA,     0x04030000
    .equ DST_RECT_VA,     0x04030010
    .equ INFO_VA,         0x04030020  @ GetScreenInfo out / pen out slots
    .equ PIXMAP_DATA4_VA, 0x04021200  @ rotated blit source (blit D)
    .equ FB_VA,           0x0E000000
    .equ BLIT_CODE,       0x0407      @ driver=4, subfn=7
    .equ INFO_CODE,       0x0403      @ driver=4, subfn=3 GetScreenInfo
    .equ GETF_CODE,       0x0408      @ driver=4, subfn=8 GetFeature
    .equ SETF_CODE,       0x0409      @ driver=4, subfn=9 SetFeature
    .equ DRAIN_CODE,      0x0516      @ driver=5, subfn=0x16 NativeGetSample
    @ Injected portrait-panel pen sample (x=10, y=20, pressure=4) and
    @ its expected screen-space rotation under orientation 1
    @ (xs = 480-1-y = 459, ys = x = 10).
    .equ PEN_IN,          0x01400A04  @ (10<<21)|(20<<7)|4
    .equ PEN_OUT,         0x39600504  @ (459<<21)|(10<<7)|4

    .section .text, "ax"
    .global test_main
test_main:
    @ ---- Build the PixelMap struct ----
    ldr     r6, =PIXMAP_VA

    ldr     r0, =PIXMAP_DATA_VA
    str     r0, [r6, #0x00]          @ addy

    @ rowBytes = 4, packed in HIGH 16 bits (Newton/Einstein layout).
    mov     r0, #4
    lsl     r0, r0, #16
    str     r0, [r6, #0x04]          @ rowBytes_packed = (4<<16)

    @ bounds = {top=0, left=0, bottom=2, right=16}.
    @ Word 0 = (top<<16) | left = 0
    mov     r0, #0
    str     r0, [r6, #0x08]
    @ Word 1 = (bottom<<16) | right = (2<<16)|16
    mov     r0, #2
    lsl     r0, r0, #16
    orr     r0, r0, #16
    str     r0, [r6, #0x0C]
    mov     r0, #0
    str     r0, [r6, #0x10]          @ flags
    str     r0, [r6, #0x14]          @ table

    @ ---- Plant bitmap bytes (8 bytes, 2 rows * 4 bytes) ----
    @ The screen.blit handler reads source pixmap bytes in BE-32
    @ word-invariant order (read_byte_pa(pa ^ 3)) — the same
    @ convention inline_patch uses for guest LDRB. To feed it the
    @ logical byte stream "DE AD BE EF / CA FE BA BE", plant the
    @ STR'd word as the BE-byte-stream encoded as an integer:
    @   STR 0xDEADBEEF → host LE bytes EF BE AD DE
    @                  → BE-view bytes DE AD BE EF (handler reads).
    ldr     r6, =PIXMAP_DATA_VA
    ldr     r0, =0xDEADBEEF          @ BE-view bytes DE AD BE EF
    str     r0, [r6, #0]
    ldr     r0, =0xCAFEBABE          @ BE-view bytes CA FE BA BE
    str     r0, [r6, #4]

    @ ---- Build src + dst rects (same shape, top-left dst) ----
    ldr     r6, =SRC_RECT_VA
    mov     r0, #0
    str     r0, [r6, #0]
    mov     r0, #2
    lsl     r0, r0, #16
    orr     r0, r0, #16
    str     r0, [r6, #4]

    ldr     r6, =DST_RECT_VA
    mov     r0, #0
    str     r0, [r6, #0]
    mov     r0, #2
    lsl     r0, r0, #16
    orr     r0, r0, #16
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
    hvc     #HVC_MARK                       @ mark 1: blit returned success

    @ ---- Verify FB bytes ----
    @ Source row 0 = DE AD BE EF, copied verbatim, host LE word 0xEFBEADDE.
    ldr     r6, =FB_VA
    ldr     r0, [r6, #0]
    ldr     r1, =0xEFBEADDE
    cmp     r0, r1
    bne     .Lfail_fb0

    @ Source row 1 = CA FE BA BE, copied verbatim, host LE word 0xBEBAFECA.
    @ FB stride is SCREEN_WIDTH*2/8 = 80 bytes, so row 1 lives at +80.
    ldr     r0, [r6, #80]
    ldr     r1, =0xBEBAFECA
    cmp     r0, r1
    bne     .Lfail_fb1

    mov     r0, #2
    hvc     #HVC_MARK                       @ mark 2: FB has expected bytes

    @ ==== Blit B: mode 1 (max/ink merge) over a sub-byte dst rect ====
    @ Source: 1 row x 4 pixels, byte 0x3D = pixels (0,3,3,1).
    @ Dst rect {0,2,1,6} — dst_left=2 is not a multiple of 4, so this
    @ takes the slow byte-merge path; mode 1 combines per pixel as
    @ max(src, dst) over the FB content blit A left behind:
    @   FB byte0 0xDE = (3,1,3,2), byte1 0xAD = (2,2,3,1)
    @   dst pix 2..5 = (3,2,2,2) max src (0,3,3,1) → (3,3,3,2)
    @ → byte0 (3,1,3,3) = 0xDF, byte1 (3,2,3,1) = 0xED, and the FB
    @ word at +0 reads back 0xEFBEEDDF.
    ldr     r6, =PIXMAP_VA
    ldr     r0, =PIXMAP_DATA2_VA
    str     r0, [r6, #0x00]          @ addy → merge source bytes

    ldr     r6, =PIXMAP_DATA2_VA
    ldr     r0, =0x3D000000          @ BE-view byte 0 = 0x3D
    str     r0, [r6, #0]

    ldr     r6, =SRC_RECT_VA
    mov     r0, #0
    str     r0, [r6, #0]             @ src {top=0, left=0}
    mov     r0, #1
    lsl     r0, r0, #16
    orr     r0, r0, #4
    str     r0, [r6, #4]             @ src {bottom=1, right=4}

    ldr     r6, =DST_RECT_VA
    mov     r0, #2
    str     r0, [r6, #0]             @ dst {top=0, left=2}
    mov     r0, #1
    lsl     r0, r0, #16
    orr     r0, r0, #6
    str     r0, [r6, #4]             @ dst {bottom=1, right=6}

    @ Mode word lives at [SP+4] (the native ABI's 4th argument slot).
    sub     sp, sp, #8
    mov     r0, #1
    str     r0, [sp, #4]             @ mode = 1 (darken-only merge)
    ldr     r1, =PIXMAP_VA
    ldr     r2, =SRC_RECT_VA
    ldr     r3, =DST_RECT_VA
    ldr     r5, =BLIT_CODE
    .inst   0xEE005A10               @ MCR p10, 0, r5, c0, c0, 0
    add     sp, sp, #8

    cmp     r0, #0
    bne     .Lfail_not_zero

    ldr     r6, =FB_VA
    ldr     r0, [r6, #0]
    ldr     r1, =0xEFBEEDDF
    cmp     r0, r1
    bne     .Lfail_fb_merge

    mov     r0, #3
    hvc     #HVC_MARK                       @ mark 3: mode-1 merge correct

    @ ==== Blit C: aligned fast path with a page-crossing source ====
    @ Source row = 16 bytes at 0x04021FF8..0x04022008, straddling the
    @ 4 KiB boundary at 0x04022000 so the per-page span walk has to
    @ split the row into two segments. Copied verbatim (mode 0) into
    @ FB row 4 (byte offset 4 * 80 = 320).
    ldr     r6, =PIXMAP_VA
    ldr     r0, =PIXMAP_DATA3_VA
    str     r0, [r6, #0x00]          @ addy → page-straddling bytes
    mov     r0, #16
    lsl     r0, r0, #16
    str     r0, [r6, #0x04]          @ rowBytes_packed = (16<<16)

    ldr     r6, =PIXMAP_DATA3_VA
    ldr     r0, =0x01234567          @ BE-view bytes 01 23 45 67
    str     r0, [r6, #0]
    ldr     r0, =0x89ABCDEF          @ BE-view bytes 89 AB CD EF
    str     r0, [r6, #4]
    ldr     r0, =0x11223344          @ BE-view bytes 11 22 33 44
    str     r0, [r6, #8]
    ldr     r0, =0x55667788          @ BE-view bytes 55 66 77 88
    str     r0, [r6, #12]

    ldr     r6, =SRC_RECT_VA
    mov     r0, #0
    str     r0, [r6, #0]             @ src {top=0, left=0}
    mov     r0, #1
    lsl     r0, r0, #16
    orr     r0, r0, #64
    str     r0, [r6, #4]             @ src {bottom=1, right=64}

    ldr     r6, =DST_RECT_VA
    mov     r0, #4
    lsl     r0, r0, #16
    str     r0, [r6, #0]             @ dst {top=4, left=0}
    mov     r0, #5
    lsl     r0, r0, #16
    orr     r0, r0, #64
    str     r0, [r6, #4]             @ dst {bottom=5, right=64}

    sub     sp, sp, #8
    mov     r0, #0
    str     r0, [sp, #4]             @ mode = 0 (srcCopy → fast path)
    ldr     r1, =PIXMAP_VA
    ldr     r2, =SRC_RECT_VA
    ldr     r3, =DST_RECT_VA
    ldr     r5, =BLIT_CODE
    .inst   0xEE005A10               @ MCR p10, 0, r5, c0, c0, 0
    add     sp, sp, #8

    cmp     r0, #0
    bne     .Lfail_not_zero

    @ FB row 4 starts at +320: first word = bytes 01 23 45 67
    @ (host LE 0x67452301), last word at +332 = bytes 55 66 77 88
    @ (host LE 0x88776655) — the tail lands past the source's page
    @ boundary, proving the second segment copied too.
    ldr     r6, =FB_VA
    ldr     r0, [r6, #320]
    ldr     r1, =0x67452301
    cmp     r0, r1
    bne     .Lfail_fb_cross0
    ldr     r0, [r6, #332]
    ldr     r1, =0x88776655
    cmp     r0, r1
    bne     .Lfail_fb_cross1

    mov     r0, #4
    hvc     #HVC_MARK                       @ mark 4: page-crossing copy correct

    @ ==== Orientation: SetFeature(4, 1) → landscape ====
    @ Portrait baseline is 0; bit 0 set = landscape (the MP2x00
    @ native orientation the ROM asserts at UI start).
    mov     r1, #4                   @ feature 4 = orientation
    mov     r2, #1                   @ value 1 = landscape
    ldr     r5, =SETF_CODE
    .inst   0xEE005A10
    cmp     r0, #0
    bne     .Lfail_not_zero

    @ GetFeature(4) reads the stored orientation back.
    mov     r1, #4
    ldr     r5, =GETF_CODE
    .inst   0xEE005A10
    cmp     r0, #1
    bne     .Lfail_orient_get

    @ GetScreenInfo now reports the swapped geometry:
    @ height=320 @ +0, width=480 @ +4.
    ldr     r1, =INFO_VA
    ldr     r5, =INFO_CODE
    .inst   0xEE005A10
    cmp     r0, #0
    bne     .Lfail_not_zero
    ldr     r6, =INFO_VA
    ldr     r0, [r6, #0]
    ldr     r1, =320
    cmp     r0, r1
    bne     .Lfail_info_h
    ldr     r0, [r6, #4]
    ldr     r1, =480
    cmp     r0, r1
    bne     .Lfail_info_w

    mov     r0, #5
    hvc     #HVC_MARK                       @ mark 5: orientation stored + info swapped

    @ ==== Blit D: rotated blit under orientation 1 ====
    @ Source: 1 row x 4 px, byte 0xE4 = pixels (3,2,1,0), blitted to
    @ landscape dst {0,0,1,4}. Orientation 1 maps screen pixel
    @ (xs, ys) to portrait (ys, 479-xs), so (xs, 0) lands at column
    @ 0 of row 479-xs: FB byte offset (479-xs)*80, pixel slot 0
    @ (bits 7..6). Expect FB[38320]=0xC0 (pixel 3), FB[38240]=0x80
    @ (2), FB[38160]=0x40 (1), FB[38080]=0x00 (0).
    ldr     r6, =PIXMAP_VA
    ldr     r0, =PIXMAP_DATA4_VA
    str     r0, [r6, #0x00]          @ addy → rotated-blit source
    mov     r0, #4
    lsl     r0, r0, #16
    str     r0, [r6, #0x04]          @ rowBytes_packed = (4<<16)

    ldr     r6, =PIXMAP_DATA4_VA
    ldr     r0, =0xE4000000          @ BE-view byte 0 = 0xE4
    str     r0, [r6, #0]

    ldr     r6, =SRC_RECT_VA
    mov     r0, #0
    str     r0, [r6, #0]             @ src {top=0, left=0}
    mov     r0, #1
    lsl     r0, r0, #16
    orr     r0, r0, #4
    str     r0, [r6, #4]             @ src {bottom=1, right=4}

    ldr     r6, =DST_RECT_VA
    mov     r0, #0
    str     r0, [r6, #0]             @ dst {top=0, left=0}
    mov     r0, #1
    lsl     r0, r0, #16
    orr     r0, r0, #4
    str     r0, [r6, #4]             @ dst {bottom=1, right=4}

    sub     sp, sp, #8
    mov     r0, #0
    str     r0, [sp, #4]             @ mode = 0
    ldr     r1, =PIXMAP_VA
    ldr     r2, =SRC_RECT_VA
    ldr     r3, =DST_RECT_VA
    ldr     r5, =BLIT_CODE
    .inst   0xEE005A10               @ MCR p10, 0, r5, c0, c0, 0
    add     sp, sp, #8
    cmp     r0, #0
    bne     .Lfail_not_zero

    @ Each target byte is host byte 0 of its aligned FB word (bits
    @ 7..0 of the LDR); the other three bytes were never written.
    ldr     r6, =FB_VA
    ldr     r1, =38320
    ldr     r0, [r6, r1]
    cmp     r0, #0xC0
    bne     .Lfail_rot0
    ldr     r1, =38240
    ldr     r0, [r6, r1]
    cmp     r0, #0x80
    bne     .Lfail_rot1
    ldr     r1, =38160
    ldr     r0, [r6, r1]
    cmp     r0, #0x40
    bne     .Lfail_rot2

    mov     r0, #6
    hvc     #HVC_MARK                       @ mark 6: rotated blit landed transposed

    @ ==== Pen sample rotated into screen space ====
    @ Inject a portrait-panel sample; NativeGetSample must return it
    @ rotated into the landscape screen space (PEN_IN → PEN_OUT).
    ldr     r0, =PEN_IN
    ldr     r1, =0x00012345
    hvc     #HVC_INJECT_PEN
    ldr     r1, =INFO_VA             @ reuse as sample/ticks out slots
    add     r2, r1, #4
    ldr     r5, =DRAIN_CODE
    .inst   0xEE005A10
    cmp     r0, #1
    bne     .Lfail_pen_drain
    ldr     r0, [r1]
    ldr     r3, =PEN_OUT
    cmp     r0, r3
    bne     .Lfail_pen_val

    mov     r0, #7
    hvc     #HVC_MARK                       @ mark 7: pen sample rotated

    @ ==== Restore the portrait baseline and confirm ====
    mov     r1, #4
    mov     r2, #0
    ldr     r5, =SETF_CODE
    .inst   0xEE005A10
    cmp     r0, #0
    bne     .Lfail_not_zero
    ldr     r1, =INFO_VA
    ldr     r5, =INFO_CODE
    .inst   0xEE005A10
    ldr     r6, =INFO_VA
    ldr     r0, [r6, #0]
    ldr     r1, =480
    cmp     r0, r1
    bne     .Lfail_info_restore

    mov     r0, #8
    hvc     #HVC_MARK                       @ mark 8: portrait restored

    hvc     #HVC_PASS                       @ PASS
    bx      lr

.Lfail_not_zero:
    hvc     #HVC_PRINT_HEX                       @ print observed r0
    mov     r0, #0xD0
    hvc     #HVC_FAIL
.Lfail_fb0:
    hvc     #HVC_PRINT_HEX                       @ print observed value
    mov     r0, #0xD1
    hvc     #HVC_FAIL
.Lfail_fb1:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xD2
    hvc     #HVC_FAIL
.Lfail_fb_merge:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xD3
    hvc     #HVC_FAIL
.Lfail_fb_cross0:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xD4
    hvc     #HVC_FAIL
.Lfail_fb_cross1:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xD5
    hvc     #HVC_FAIL
.Lfail_orient_get:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xD6
    hvc     #HVC_FAIL
.Lfail_info_h:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xD7
    hvc     #HVC_FAIL
.Lfail_info_w:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xD8
    hvc     #HVC_FAIL
.Lfail_rot0:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xD9
    hvc     #HVC_FAIL
.Lfail_rot1:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xDA
    hvc     #HVC_FAIL
.Lfail_rot2:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xDB
    hvc     #HVC_FAIL
.Lfail_pen_drain:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xDC
    hvc     #HVC_FAIL
.Lfail_pen_val:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xDD
    hvc     #HVC_FAIL
.Lfail_info_restore:
    hvc     #HVC_PRINT_HEX
    mov     r0, #0xDE
    hvc     #HVC_FAIL
