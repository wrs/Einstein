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
    hvc     #1
    mov     r0, #'E'
    hvc     #1
    mov     r0, #'L'
    hvc     #1
    mov     r0, #'L'
    hvc     #1
    mov     r0, #'O'
    hvc     #1
    mov     r0, #'\n'
    hvc     #1

    @ Print a recognizable magic word so the hypervisor's hex-print
    @ path is exercised too.
    ldr     r0, =0xCAFEBABE
    hvc     #2

    hvc     #3                  @ pass
    bx      lr
