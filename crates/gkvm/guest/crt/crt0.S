/* gk-guest-crt entry: the SP1 v6 entry contract, in plain gas syntax.
 * The executor starts at e_entry with every register zero; the guest owns
 * its stack pointer (STACK_TOP = 0x78000000, growing down) and its global
 * pointer. Memory is zero-initialized by the executor, so .bss needs no
 * clearing here. __gk_start (gkvm.c) reads the artifact root, calls main,
 * and halts with main's return value.
 */
    .section .text._start
    .globl _start
_start:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    li sp, 0x78000000
    call __gk_start
