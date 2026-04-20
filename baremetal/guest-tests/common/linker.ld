/* baremetal/guest-tests/common/linker.ld
 *
 * Tiny linker script for guest test binaries.  Image loads at guest
 * IPA 0x0, so the vector table (`.text.vectors` in test_runtime.S)
 * must be the very first thing. `.text.vectors` is linked before
 * any other `.text` content; then the rest of `.text`, then rodata
 * and bss.
 */

ENTRY(_start)

SECTIONS
{
    . = 0;
    .text : {
        KEEP(*(.text.vectors))
        *(.text .text.*)
    }

    .rodata : { *(.rodata .rodata.*) }
    .data   : { *(.data .data.*) }
    .bss    : { *(.bss .bss.*) *(COMMON) }

    /DISCARD/ : { *(.ARM.*) *(.note.*) *(.comment) }
}
