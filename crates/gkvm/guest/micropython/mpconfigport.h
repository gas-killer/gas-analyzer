/* MicroPython configuration for the gkvm port.
 *
 * A gkvm guest is a pure function of (program, payload, artifact): no clock,
 * no randomness, no filesystem, no network, no console. Everything here
 * follows from that — every module that needs a HAL the hostcall surface
 * does not have is off, and the interpreter runs frozen bytecode only.
 */
#include <stdint.h>

#define MICROPY_CONFIG_ROM_LEVEL (MICROPY_CONFIG_ROM_LEVEL_EXTRA_FEATURES)

/* Frozen bytecode only: the guest .py is compiled by mpy-cross at build time
 * and linked into the ELF, so programHash commits to it. No compiler in the
 * image means no eval/exec/compile and no source imports. py/mkrules.mk
 * defines MICROPY_MODULE_FROZEN_STR whenever a manifest is set; frozen source
 * needs the compiler, so it is switched back off here.
 * MICROPY_ENABLE_EXTERNAL_IMPORT is what py/builtinimport.c gates the FROZEN
 * module lookup on (without it `import` finds built-in modules only), so it is
 * on — with a filesystem that has nothing in it (mp_import_stat, main.c), and
 * no way to load what it would find (no compiler, no .mpy loader). */
#define MICROPY_ENABLE_COMPILER (0)
#undef MICROPY_MODULE_FROZEN_STR
#define MICROPY_MODULE_FROZEN_STR (0)
#define MICROPY_ENABLE_EXTERNAL_IMPORT (1)
#define MICROPY_PERSISTENT_CODE_LOAD (0)

/* Integers only. The target is rv64im: no F/D, and soft-float results are a
 * libgcc implementation detail a consensus count must not depend on. */
#define MICROPY_FLOAT_IMPL (MICROPY_FLOAT_IMPL_NONE)
#define MICROPY_PY_BUILTINS_COMPLEX (0)
#define MICROPY_LONGINT_IMPL (MICROPY_LONGINT_IMPL_MPZ)

#define MICROPY_ENABLE_GC (1)
#define MICROPY_GCREGS_SETJMP (0)
#define MICROPY_NLR_SETJMP (0)
#define MICROPY_STACK_CHECK (1)
#define MICROPY_ENABLE_SOURCE_LINE (1)
#define MICROPY_OPT_COMPUTED_GOTO (1)
#define MICROPY_USE_INTERNAL_ERRNO (1)
#define MICROPY_USE_INTERNAL_PRINTF (1)

/* No console: print() is accepted and dropped (see mp_hal_stdout_tx_strn). */
#define MICROPY_HELPER_REPL (0)
#define MICROPY_REPL_EMACS_KEYS (0)
#define MICROPY_REPL_AUTO_INDENT (0)
#define MICROPY_KBD_EXCEPTION (0)
#define MICROPY_PY_BUILTINS_INPUT (0)
#define MICROPY_PY_BUILTINS_HELP (0)
#define MICROPY_PY_SYS_STDFILES (0)
#define MICROPY_PY_SYS_STDIO_BUFFER (0)
#define MICROPY_PY_SYS_PS1_PS2 (0)

/* No clock, entropy, filesystem, devices or event loop. */
#define MICROPY_PY_TIME (0)
#define MICROPY_PY_RANDOM (0)
#define MICROPY_PY_OS (0)
#define MICROPY_PY_SELECT (0)
#define MICROPY_PY_MACHINE (0)
#define MICROPY_PY_ASYNCIO (0)
#define MICROPY_PY_UCTYPES (0)
/* platform probes the misa CSR on RISC-V: not rv64im, a fault under SP1. */
#define MICROPY_PY_PLATFORM (0)
#define MICROPY_PY_FRAMEBUF (0)
#define MICROPY_VFS (0)
#define MICROPY_READER_VFS (0)
#define MICROPY_ENABLE_SCHEDULER (0)

#define MICROPY_PY_SYS_PLATFORM "gkvm"
#define MICROPY_HW_BOARD_NAME "gkvm"
#define MICROPY_HW_MCU_NAME "rv64im"

#define MP_SSIZE_MAX (0x7fffffffffffffff)
typedef long mp_off_t;

#include <alloca.h>

#define MP_STATE_PORT MP_STATE_VM
