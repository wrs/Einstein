/*
 * Link layout for the Newton hypervisor on FVP_Base_RevC-2xAEMvA.
 *
 * FVP loads an ELF via `-a cluster0.cpu0=image.elf` by program-header PA.
 * Non-secure DRAM on the Base Platform memory map starts at 0x80000000
 * and is 2 GiB deep; we land the image at DRAM base and set the FVP's
 * cluster0.cpu0.RVBAR to match, giving a direct EL2 entry when
 * cluster0.has_el3=0.
 *
 * Layout mirrors linker.ld (raspi3b) — only the load address differs.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x80000000;
    __image_start = .;

    .text.boot : { KEEP(*(.text.boot)) }
    .text      : { *(.text .text.*) }

    . = ALIGN(16);
    .rodata    : { *(.rodata .rodata.*) }

    . = ALIGN(16);
    .data      : { *(.data .data.*) }

    . = ALIGN(16);
    __bss_start = .;
    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(COMMON)
    }
    . = ALIGN(16);
    __bss_end = .;

    . = ALIGN(4096);
    /*
     * EL2 stack for core 0. The lowest 8 bytes hold a guard canary
     * (__stack_guard), seeded in boot.s and checked in trap_irq + the
     * halt paths: if the stack ever descends into it, the magic word is
     * clobbered and the next check loud-halts instead of letting an
     * overflow silently corrupt .bss below. Usable stack is therefore
     * (__stack_guard + 8 .. __stack_top].
     */
    __stack_guard = .;
    . = . + 0x4000;   /* 16 KiB stack for core 0 (incl. the guard word). */
    __stack_top = .;

    __image_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
    }
}
