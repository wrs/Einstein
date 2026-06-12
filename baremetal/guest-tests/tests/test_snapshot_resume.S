@ tests/test_snapshot_resume.S
@
@ Two-run automation of the snapshot save/resume workflow. Driven by
@ guest-tests/scripts/run-snapshot-resume.sh (special-cased in
@ run-all.sh), NOT the normal one-shot loop:
@
@   Run 1 (cold, snapshots cleared): execute the pre-save block, which
@     plants a distinctive GPR + RAM pattern and issues HVC #0x18 (HVC_SNAPSHOT)
@     (HVC_SNAPSHOT). The hypervisor saves x0..x30 + RAM at the
@     post-HVC PC. Run 1 then falls through into the post-resume
@     checkpoint, verifies the pattern it just set, and PASSes.
@
@   Run 2 (snapshots present, same binary): the hypervisor's
@     load_latest() matches the ROM fingerprint (first 1 KiB of the
@     identical test image) and ERETs straight to the saved PC — the
@     instruction right after the HVC_SNAPSHOT — with all GPRs and RAM
@     restored. It does NOT re-run _reset. The driver asserts both the
@     "Resuming guest from snapshot" banner AND a guest test PASS, so
@     the post-resume register/RAM check is what proves execution
@     continued correctly past the resume (not merely that the banner
@     printed).
@
@ The checkpoint is reachable two ways (fall-through on run 1, ERET
@ landing on run 2); both must verify the same pattern, so a corrupt
@ restore on run 2 fails the GPR/RAM comparison and the run reports
@ FAILED.

#include "../common/test_runtime.S"

    @ RAM sentinel slot (stage-2 RW, below the SVC stack and clear of
    @ the irq_count/irq_pending slots the runtime uses at 0x04000100).
    .equ SENTINEL_ADDR, 0x04000200
    .equ SENTINEL_VAL,  0xC0FFEE42

    @ Distinctive GPR pattern planted pre-save and checked post-resume.
    .equ PAT_R4,  0xA4A4A4A4
    .equ PAT_R5,  0x5B5B5B5B
    .equ PAT_R6,  0x6C6C6C6C
    .equ PAT_R7,  0x7D7D7D7D

    .global test_main
test_main:
    @ ---- pre-save: plant the pattern ----------------------------
    ldr     r4, =PAT_R4
    ldr     r5, =PAT_R5
    ldr     r6, =PAT_R6
    ldr     r7, =PAT_R7

    ldr     r0, =SENTINEL_ADDR
    ldr     r1, =SENTINEL_VAL
    str     r1, [r0]

    @ Save. ELR_EL2 at the trap points at the instruction AFTER this
    @ HVC, so run 2 resumes at `.Lcheckpoint`.
    hvc     #HVC_SNAPSHOT

.Lcheckpoint:
    @ ---- post-resume checkpoint (also the run-1 fall-through) ----
    @ Prove the restored GPRs survived.
    ldr     r0, =PAT_R4
    cmp     r4, r0
    bne     .Lfail_r4
    ldr     r0, =PAT_R5
    cmp     r5, r0
    bne     .Lfail_r5
    ldr     r0, =PAT_R6
    cmp     r6, r0
    bne     .Lfail_r6
    ldr     r0, =PAT_R7
    cmp     r7, r0
    bne     .Lfail_r7

    @ Prove the restored RAM sentinel survived.
    ldr     r0, =SENTINEL_ADDR
    ldr     r1, [r0]
    ldr     r2, =SENTINEL_VAL
    cmp     r1, r2
    bne     .Lfail_ram

    mov     r0, #1
    hvc     #HVC_MARK                  @ mark 1: pattern verified

    hvc_pass
    bx      lr

.Lfail_r4:
    hvc_hex32 r4
    hvc_fail 0xD0
.Lfail_r5:
    hvc_hex32 r5
    hvc_fail 0xD1
.Lfail_r6:
    hvc_hex32 r6
    hvc_fail 0xD2
.Lfail_r7:
    hvc_hex32 r7
    hvc_fail 0xD3
.Lfail_ram:
    hvc_hex32 r1
    hvc_fail 0xD4
