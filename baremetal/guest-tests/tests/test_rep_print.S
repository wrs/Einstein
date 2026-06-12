@ tests/test_rep_print.S
@
@ Pins the REP / Hammer print VaArgs + format-rendering ABI that
@ `src/rep_print.rs` implements behind the HammerPrint/Putc/Flush HVCs.
@ On a real boot those bytes go to the host UART, which a guest test
@ can't read back, so the harness exposes a test-only HVC
@ (`HVC_REP_RENDER`, src/trap/hvc.rs under cfg(nh_guest_test)) that runs
@ the *same* `rep_print::render_into` interpreter into a guest-supplied
@ buffer and returns the rendered length in r0. The guest then
@ byte-compares the result, asserting the ABI guest-side.
@
@ HVC_REP_RENDER ABI:
@   r0 = format string ptr   r1 = out buffer ptr
@   r2 = vararg0  r3 = vararg1  [SP+0..] = vararg2..
@   -> r0 = number of bytes rendered (output NOT NUL-terminated)
@
@ This exercises the VaArgs path (r2/r3 then the source-mode SVC
@ stack), `%s`, width via `%*d`, and the Phase-1-fixed precision-
@ consume `%.*s` (the precision argument must be consumed so the
@ following specifier stays aligned).
@
@ STRING LAYOUT: the renderer (and the kernel's own `read_cstr_at`)
@ read guest C-strings in Newton's BE-byte-within-LE-word order — byte
@ k of the string lives at `(word >> ((3-k)*8))`. We therefore stage
@ every format / argument string in RAM at runtime as 32-bit words with
@ the four chars packed big-endian (char 0 in the high byte). A guest
@ `strb`-built buffer would land in the opposite lane and the renderer
@ would read it scrambled — exactly the bug class this test guards.

#include "../common/test_runtime.S"

    @ Scratch RAM region (stage-2 RW, below the SVC stack at 0x04001000,
    @ clear of the runtime's irq_count/irq_pending at 0x04000100).
    .equ STRBASE, 0x04000200         @ staged strings
    .equ OUTBUF,  0x04000300         @ rendered-output buffer
    .equ EXPBUF,  0x04000380         @ expected-output buffer (BE-packed)

    @ Pack four chars big-endian into a word and store at STRBASE+off.
    @ Clobbers r0/r1.
    .macro put_word off, c0, c1, c2, c3
        ldr     r1, =((\c0 << 24) | (\c1 << 16) | (\c2 << 8) | \c3)
        ldr     r0, =(STRBASE + \off)
        str     r1, [r0]
    .endm

    @ Pack four chars big-endian into a word and store at EXPBUF+off.
    .macro exp_word off, c0, c1, c2, c3
        ldr     r1, =((\c0 << 24) | (\c1 << 16) | (\c2 << 8) | \c3)
        ldr     r0, =(EXPBUF + \off)
        str     r1, [r0]
    .endm

    @ String byte offsets within STRBASE (word-aligned starts).
    .equ S_FMT_S,   0                @ "x=%s!"   (6 bytes)
    .equ S_STR_HI,  8                @ "hi"      (3 bytes)
    .equ S_FMT_WD,  12               @ "[%*d]"   (6 bytes)
    .equ S_FMT_PS,  20               @ "%.*s%d"  (7 bytes)
    .equ S_STR_ABC, 28               @ "abc"     (4 bytes)

    .global test_main
test_main:
    @ ---- stage all strings into RAM (BE-packed words) ----------
    put_word S_FMT_S,   'x', '=', '%', 's'      @ "x=%s"
    put_word (S_FMT_S+4), '!', 0,  0,  0        @ "!\0"
    put_word S_STR_HI,  'h', 'i', 0,  0         @ "hi\0"
    put_word S_FMT_WD,  '[', '%', '*', 'd'      @ "[%*d"
    put_word (S_FMT_WD+4), ']', 0,  0,  0       @ "]\0"
    put_word S_FMT_PS,  '%', '.', '*', 's'      @ "%.*s"
    put_word (S_FMT_PS+4), '%', 'd', 0,  0      @ "%d\0"
    put_word S_STR_ABC, 'a', 'b', 'c', 0        @ "abc\0"

    @ ============================================================
    @ Sub-test 1: "%s" — vararg0 = ptr to "hi".
    @   fmt = "x=%s!"  -> expect "x=hi!" (5 bytes)
    @ ============================================================
    @ Expected output, BE-packed: "x=hi" | "!\0\0\0"
    exp_word 0, 'x', '=', 'h', 'i'
    exp_word 4, '!', 0,  0,  0

    bl      zero_out
    ldr     r0, =(STRBASE + S_FMT_S)
    ldr     r1, =OUTBUF
    ldr     r2, =(STRBASE + S_STR_HI)        @ vararg0 = string pointer
    mov     r3, #0
    hvc     #HVC_REP_RENDER

    cmp     r0, #5
    bne     .Lfail_len_1
    mov     r2, #5
    bl      check_out
    cmp     r0, #0
    bne     .Lfail_cmp_1
    mov     r0, #1
    hvc     #HVC_MARK

    @ ============================================================
    @ Sub-test 2: width "%*d" — vararg0 = width 5, vararg1 = 42.
    @   fmt = "[%*d]"  -> expect "[   42]" (7 bytes)
    @ ============================================================
    @ Expected output, BE-packed: "[   " | "42]\0"
    exp_word 0, '[', ' ', ' ', ' '
    exp_word 4, '4', '2', ']', 0

    bl      zero_out
    ldr     r0, =(STRBASE + S_FMT_WD)
    ldr     r1, =OUTBUF
    mov     r2, #5                           @ vararg0 = width
    mov     r3, #42                          @ vararg1 = value
    hvc     #HVC_REP_RENDER

    cmp     r0, #7
    bne     .Lfail_len_2
    mov     r2, #7
    bl      check_out
    cmp     r0, #0
    bne     .Lfail_cmp_2
    mov     r0, #2
    hvc     #HVC_MARK

    @ ============================================================
    @ Sub-test 3: precision-consume "%.*s" then "%d".
    @   fmt  = "%.*s%d"
    @   vararg0 = precision (3, consumed+ignored),
    @   vararg1 = ptr to "abc",
    @   vararg2 = 7  (must come from the SVC stack at [SP+0])
    @   The precision arg MUST be consumed so %d reads vararg2=7, not
    @   the "abc" pointer. Expect "abc7" (4 bytes); precision is ignored
    @   by the renderer so the whole "abc" prints.
    @ ============================================================
    @ Expected output, BE-packed: "abc7" | "\0\0\0\0"
    exp_word 0, 'a', 'b', 'c', '7'
    exp_word 4, 0,   0,   0,   0

    bl      zero_out
    ldr     r0, =(STRBASE + S_FMT_PS)
    ldr     r1, =OUTBUF
    mov     r2, #3                           @ vararg0 = precision
    ldr     r3, =(STRBASE + S_STR_ABC)       @ vararg1 = string pointer
    mov     r4, #7                           @ vararg2 (on the stack)
    push    {r4}                             @ [SP+0] = 7
    hvc     #HVC_REP_RENDER
    add     sp, sp, #4                       @ discard the pushed vararg

    cmp     r0, #4
    bne     .Lfail_len_3
    mov     r2, #4
    bl      check_out
    cmp     r0, #0
    bne     .Lfail_cmp_3
    mov     r0, #3
    hvc     #HVC_MARK

    hvc_pass
    bx      lr

@ ---- helper: word-compare the two 8-byte (2-word) buffers OUTBUF and
@      EXPBUF. r2 is the rendered byte length (used only for the caller's
@      length assertion; the compare is always two words).
@
@      Why words, not bytes: the renderer writes logical byte i of its
@      output to physical offset i^3 (Newton BE-within-LE-word lane, the
@      `guest_write_u8_va` transform). A guest `ldrb` reads raw physical
@      bytes (no XOR), so a per-byte compare would mis-align. But an
@      aligned `ldr` reads the whole word, and the renderer's per-lane
@      writes reassemble into exactly the BE-packed word the test built
@      in EXPBUF — so word reads compare cleanly on both sides. The
@      caller zeroes OUTBUF first, so trailing unwritten lanes read 0,
@      matching EXPBUF's NUL padding.
@
@      Returns r0 = 0 if both words match, else r0 = 1. Clobbers r3, r12.
check_out:
    ldr     r0, =OUTBUF
    ldr     r1, =EXPBUF
    ldr     r3, [r0]                         @ OUTBUF word 0
    ldr     r12, [r1]                        @ EXPBUF word 0
    cmp     r3, r12
    bne     .Lco_ne
    ldr     r3, [r0, #4]                     @ OUTBUF word 1
    ldr     r12, [r1, #4]                    @ EXPBUF word 1
    cmp     r3, r12
    bne     .Lco_ne
    mov     r0, #0
    bx      lr
.Lco_ne:
    mov     r0, #1
    bx      lr

@ ---- helper: zero the two output words. Clobbers r0, r1. ----
zero_out:
    ldr     r0, =OUTBUF
    mov     r1, #0
    str     r1, [r0]
    str     r1, [r0, #4]
    bx      lr

.Lfail_len_1:
    hvc_hex32 r0
    hvc_fail 0xA0
.Lfail_cmp_1:
    hvc_fail 0xA1
.Lfail_len_2:
    hvc_hex32 r0
    hvc_fail 0xA2
.Lfail_cmp_2:
    hvc_fail 0xA3
.Lfail_len_3:
    hvc_hex32 r0
    hvc_fail 0xA4
.Lfail_cmp_3:
    hvc_fail 0xA5
