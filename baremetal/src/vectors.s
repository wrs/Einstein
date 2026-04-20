// EL2 vector table with full AArch64 context save/restore.
//
// On exception entry we spill x0..x30 to the stack, hand the handler a
// pointer to that saved context, and let it modify register values
// (important for emulating guest LDR results) before we restore and ERET.
//
// Layout per vector (128 bytes each, 2 KiB total):
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
//   0x600   Synchronous                       Lower EL, AArch32  *** HVC / stage-2 / undef
//   0x680   IRQ                               Lower EL, AArch32
//   0x700   FIQ                               Lower EL, AArch32
//   0x780   SError                            Lower EL, AArch32
//
// Handlers receive a `*mut TrapContext` in x0. Returning lets the trailing
// restore-and-ERET sequence run; handlers that must not resume call halt()
// instead.

// Size of TrapContext in bytes: 31 × 8 = 248, rounded to 256 for alignment.
.equ CTX_SIZE, 256

.section .text.vectors, "ax"
.balign 0x800
.global el2_vector_table

// Save x0..x30 on the stack in the layout TrapContext expects.
.macro save_context
    sub     sp, sp, #CTX_SIZE
    stp     x0,  x1,  [sp, #0]
    stp     x2,  x3,  [sp, #16]
    stp     x4,  x5,  [sp, #32]
    stp     x6,  x7,  [sp, #48]
    stp     x8,  x9,  [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    str     x30,      [sp, #240]
.endm

.macro restore_context_and_eret
    ldp     x0,  x1,  [sp, #0]
    ldp     x2,  x3,  [sp, #16]
    ldp     x4,  x5,  [sp, #32]
    ldp     x6,  x7,  [sp, #48]
    ldp     x8,  x9,  [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldr     x30,      [sp, #240]
    add     sp, sp, #CTX_SIZE
    eret
.endm

// Resumable entry: save context, pass ctx ptr, call handler, restore + ERET.
.macro entry_resume handler
    .balign 0x80
    save_context
    mov     x0, sp
    bl      \handler
    restore_context_and_eret
.endm

// Terminal entry: save context, pass ctx, call handler -> !.
.macro entry_halt handler
    .balign 0x80
    save_context
    mov     x0, sp
    bl      \handler
1:  wfe
    b       1b
.endm

el2_vector_table:
    entry_halt   trap_unexpected          // 0x000
    entry_halt   trap_unexpected          // 0x080
    entry_halt   trap_unexpected          // 0x100
    entry_halt   trap_unexpected          // 0x180
    entry_halt   trap_unexpected          // 0x200
    entry_halt   trap_unexpected          // 0x280
    entry_halt   trap_unexpected          // 0x300
    entry_halt   trap_unexpected          // 0x380
    entry_halt   trap_unexpected          // 0x400
    entry_halt   trap_unexpected          // 0x480
    entry_halt   trap_unexpected          // 0x500
    entry_halt   trap_unexpected          // 0x580
    entry_resume trap_sync_lower_aarch32  // 0x600  *** guest sync exceptions
    entry_halt   trap_unexpected          // 0x680
    entry_halt   trap_unexpected          // 0x700
    entry_halt   trap_unexpected          // 0x780
