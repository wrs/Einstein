@ tests/test_hello.S
@
@ Smoke test for the guest-side test framework itself. Prints "HELLO\n"
@ one byte at a time via HVC #1, then HVC #2 prints a hex word, then
@ HVC #3 signals pass. If the hypervisor reports "guest test PASSED",
@ the HVC trap path + test-image loader are working.

#include "../common/test_runtime.S"

    .global test_main
test_main:
    mov     r0, #'H'
    hvc     #HVC_PRINT_BYTE
    mov     r0, #'E'
    hvc     #HVC_PRINT_BYTE
    mov     r0, #'L'
    hvc     #HVC_PRINT_BYTE
    mov     r0, #'L'
    hvc     #HVC_PRINT_BYTE
    mov     r0, #'O'
    hvc     #HVC_PRINT_BYTE
    mov     r0, #'\n'
    hvc     #HVC_PRINT_BYTE

    @ Print a recognizable magic word so the hypervisor's hex-print
    @ path is exercised too.
    ldr     r0, =0xCAFEBABE
    hvc     #HVC_PRINT_HEX

    hvc     #HVC_PASS                  @ pass
    bx      lr
