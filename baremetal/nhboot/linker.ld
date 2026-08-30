/*
 * Link layout for nhboot, the Pi Zero 2 W serial-upload bootloader.
 *
 * Derived from the hypervisor's linker.ld.in, with one difference that
 * is the whole point: the link address is 0x10000000 (256 MiB), NOT
 * the 0x80000 the firmware loads kernel8.img at. boot.s copies the
 * image from wherever it was entered to this address before running
 * any Rust code, so that the bootloader can later overwrite 0x80000
 * with the hypervisor payload (which links at 0x80000) and jump to it.
 *
 * Address map at boot (see src/image.rs for the constants):
 *   0x00080000  nhboot as loaded by the firmware (dead after relocation)
 *   0x02000000  HYPERV.IMG, loaded by the firmware's `initramfs` option
 *   0x10000000  nhboot after self-relocation, + stack + bss
 */

ENTRY(_start)

SECTIONS
{
    . = 0x10000000;
    __image_start = .;

    .text.boot : { KEEP(*(.text.boot)) }
    .text      : { *(.text .text.*) }

    . = ALIGN(16);
    .rodata    : { *(.rodata .rodata.*) }

    . = ALIGN(16);
    .data      : { *(.data .data.*) }

    /*
     * End of the bytes present in the raw binary. boot.s relocates
     * exactly [__image_start, __data_end) in 16-byte pairs, so this
     * must stay 16-aligned.
     */
    . = ALIGN(16);
    __data_end = .;

    __bss_start = .;
    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(COMMON)
    }
    . = ALIGN(16);
    __bss_end = .;

    . = ALIGN(4096);
    /*
     * Stack for core 0. The lowest 8 bytes hold a guard canary
     * (__stack_guard), seeded in boot.s; the panic handler reports it
     * if it has been clobbered. Usable stack is (__stack_guard + 8 ..
     * __stack_top].
     */
    __stack_guard = .;
    . = . + 0x4000;   /* 16 KiB. */
    __stack_top = .;

    __image_end = .;

    /*
     * .eh_frame_hdr must be discarded, not merely left unreferenced: a
     * binary with no .rodata (string literals folded into .text)
     * otherwise gets .eh_frame_hdr placed at the image base, shifting
     * _start off the first byte of the raw binary.
     */
    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
    }
}
