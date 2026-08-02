//! Pure AArch64/AArch32 mechanism: boot/vector asm, MMU, CPU sysregs,
//! banked-register access, instruction decode/emit. Zero upward deps.

pub mod aarch32_emit;
pub mod arm_decode;
pub mod banked;
pub mod cpu;
pub mod mmu;
pub mod slim_isr;
pub mod trap_context;
