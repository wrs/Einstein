// EL2 vector table.
//
// ARMv8 requires this be 2 KiB-aligned and laid out as 16 entries of 128
// bytes each:
//
//   Offset  Kind                              Source
//   0x000   Synchronous                       Current EL with SP0
//   0x080   IRQ                               Current EL with SP0
//   0x100   FIQ                               Current EL with SP0
//   0x180   SError                            Current EL with SP0
//   0x200   Synchronous                       Current EL with SPx
//   0x280   IRQ                               Current EL with SPx
//   0x300   FIQ                               Current EL with SPx
//   0x380   SError                            Current EL with SPx
//   0x400   Synchronous                       Lower EL, AArch64
//   0x480   IRQ                               Lower EL, AArch64
//   0x500   FIQ                               Lower EL, AArch64
//   0x580   SError                            Lower EL, AArch64
//   0x600   Synchronous                       Lower EL, AArch32  <-- HVC from toy guest
//   0x680   IRQ                               Lower EL, AArch32
//   0x700   FIQ                               Lower EL, AArch32
//   0x780   SError                            Lower EL, AArch32
//
// For M1.5a we fill in the AArch32-from-lower-EL synchronous vector with
// a handler that calls `trap_from_guest_aarch32` and halts. Every other
// vector routes to `trap_unexpected` which panics.

.section .text.vectors, "ax"
.balign 0x800
.global el2_vector_table

.macro entry handler
    .balign 0x80
    // Save a minimal caller-saved set so the Rust handler can use them.
    // The handler never returns to the vector, so we don't bother with
    // full state preservation.
    sub     sp, sp, #16
    stp     x29, x30, [sp]
    bl      \handler
    // handlers are -> !; if one does return, hang.
1:  wfe
    b       1b
.endm

el2_vector_table:
    entry trap_unexpected           // 0x000 Current EL SP0 Sync
    entry trap_unexpected           // 0x080 Current EL SP0 IRQ
    entry trap_unexpected           // 0x100 Current EL SP0 FIQ
    entry trap_unexpected           // 0x180 Current EL SP0 SError
    entry trap_unexpected           // 0x200 Current EL SPx Sync
    entry trap_unexpected           // 0x280 Current EL SPx IRQ
    entry trap_unexpected           // 0x300 Current EL SPx FIQ
    entry trap_unexpected           // 0x380 Current EL SPx SError
    entry trap_unexpected           // 0x400 Lower EL AArch64 Sync
    entry trap_unexpected           // 0x480 Lower EL AArch64 IRQ
    entry trap_unexpected           // 0x500 Lower EL AArch64 FIQ
    entry trap_unexpected           // 0x580 Lower EL AArch64 SError
    entry trap_from_guest_aarch32   // 0x600 Lower EL AArch32 Sync  *** HVC land here
    entry trap_unexpected           // 0x680 Lower EL AArch32 IRQ
    entry trap_unexpected           // 0x700 Lower EL AArch32 FIQ
    entry trap_unexpected           // 0x780 Lower EL AArch32 SError
