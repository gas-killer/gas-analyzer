/* MicroPython on gkvm: entry, GC roots, and the failure paths.
 *
 * gk-guest-crt's __gk_start calls main(). The payload is read once, before
 * the interpreter starts (the crt allows exactly one read and wants it before
 * any artifact access), so Python sees it as plain data through gkvm.input().
 * The guest script is the frozen module GK_MPY_ENTRY, run as __main__.
 *
 * Every way out is deterministic: a clean return halts 0 with whatever
 * gkvm.output() wrote; an uncaught exception becomes a GKTRAP01 frame
 * carrying the traceback text.
 */
#include <string.h>

#include "py/bc.h"
#include "py/builtin.h"
#include "py/cstack.h"
#include "py/emitglue.h"
#include "py/frozenmod.h"
#include "py/gc.h"
#include "py/mperrno.h"
#include "py/mphal.h"
#include "py/mpprint.h"
#include "py/runtime.h"
#include "shared/runtime/gchelper.h"

#include "gkport.h"

#ifndef GK_MPY_ENTRY
#error GK_MPY_ENTRY must name the frozen guest script, e.g. "hello.py"
#endif

#if GK_MPY_HEAP_BYTES <= 0 || GK_MPY_STACK_BYTES <= 0
#error GK_MPY_HEAP_BYTES and GK_MPY_STACK_BYTES must be positive
#endif

/* The frozen heap: NOBITS, so it costs nothing in the ELF, and the executor
 * hands it over zeroed. Its size is part of the image (programHash commits
 * to it) — a guest cannot grow it at run time. */
static char gk_heap[GK_MPY_HEAP_BYTES] __attribute__((aligned(16)));

u8 gk_mpy_payload[GK_INPUT_BYTES_CAP + 8] __attribute__((aligned(8)));
u64 gk_mpy_payload_len;

_Noreturn void gk_mpy_fatal(const char *msg) {
    gk_abort(GK_MPY_TRAP_FATAL, msg, (u32)strlen(msg));
}

static void gk_mpy_run_entry(void) {
    int frozen_type;
    void *frozen_data;
    mp_find_frozen_module(GK_MPY_ENTRY, &frozen_type, &frozen_data);
    if (frozen_type != MP_FROZEN_MPY) {
        gk_mpy_fatal("entry script is not frozen in this image");
    }
    /* What pyexec does for a frozen script: run it against __main__'s
     * globals, so `if __name__ == "__main__":` behaves as under CPython. */
    const mp_frozen_module_t *frozen = frozen_data;
    mp_module_context_t *ctx = m_new_obj(mp_module_context_t);
    ctx->module.globals = mp_globals_get();
    ctx->constants = frozen->constants;
    mp_call_function_0(mp_make_function_from_proto_fun(frozen->proto_fun, ctx, NULL));
}

/* sys.exit() / sys.exit(0) / raise SystemExit is a clean stop. */
static bool gk_mpy_is_clean_exit(mp_obj_t exc) {
    if (!mp_obj_is_subclass_fast(MP_OBJ_FROM_PTR(mp_obj_get_type(exc)),
        MP_OBJ_FROM_PTR(&mp_type_SystemExit))) {
        return false;
    }
    mp_obj_t value = mp_obj_exception_get_value(exc);
    return value == mp_const_none || value == MP_OBJ_NEW_SMALL_INT(0);
}

_Noreturn static void gk_mpy_trap_exception(mp_obj_t exc) {
    bool is_exit = mp_obj_is_subclass_fast(MP_OBJ_FROM_PTR(mp_obj_get_type(exc)),
        MP_OBJ_FROM_PTR(&mp_type_SystemExit));
    u32 code = is_exit ? GK_MPY_TRAP_SYSTEM_EXIT : GK_MPY_TRAP_EXCEPTION;

    /* Formatting allocates; if the heap is what failed, say so without it. */
    nlr_buf_t nlr;
    if (nlr_push(&nlr) == 0) {
        vstr_t vstr;
        mp_print_t print;
        vstr_init_print(&vstr, 256, &print);
        mp_obj_print_exception(&print, exc);
        size_t len = vstr.len > GK_MPY_TRAP_MSG_CAP ? GK_MPY_TRAP_MSG_CAP : vstr.len;
        gk_abort(code, vstr.buf, (u32)len);
    }
    static const char fallback[] = "uncaught exception (traceback could not be formatted)";
    gk_abort(code, fallback, sizeof fallback - 1);
}

int main(void) {
    gk_mpy_payload_len = gk_input_read(gk_mpy_payload);

    mp_cstack_init_with_sp_here(GK_MPY_STACK_BYTES);
    gc_init(gk_heap, gk_heap + sizeof gk_heap);
    mp_init();

    nlr_buf_t nlr;
    if (nlr_push(&nlr) == 0) {
        gk_mpy_run_entry();
        nlr_pop();
    } else {
        mp_obj_t exc = MP_OBJ_FROM_PTR(nlr.ret_val);
        if (!gk_mpy_is_clean_exit(exc)) {
            gk_mpy_trap_exception(exc);
        }
    }
    /* No mp_deinit: halting is the cleanup, and finalisers running on the way
     * out would only add cycles nobody can observe. */
    return 0;
}

void gc_collect(void) {
    gc_collect_start();
    gc_helper_collect_regs_and_stack();
    gc_collect_end();
}

/* `import` of anything that is neither built in nor frozen: there is no
 * filesystem to find it on (frozen modules are matched before this is asked). */
mp_import_stat_t mp_import_stat(const char *path) {
    (void)path;
    return MP_IMPORT_STAT_NO_EXIST;
}

/* print() has nowhere to go: the hostcall surface is payload in, result out.
 * Accept and drop, so library code that prints still runs. */
mp_uint_t mp_hal_stdout_tx_strn(const char *str, size_t len) {
    (void)str;
    return len;
}

/* io (BytesIO/StringIO) drags open() in with it; there is no filesystem. */
static mp_obj_t gk_mpy_open(size_t n_args, const mp_obj_t *args, mp_map_t *kwargs) {
    (void)n_args, (void)args, (void)kwargs;
    mp_raise_OSError(MP_ENODEV);
}
MP_DEFINE_CONST_FUN_OBJ_KW(mp_builtin_open_obj, 1, gk_mpy_open);

void nlr_jump_fail(void *val) {
    (void)val;
    gk_mpy_fatal("uncaught NLR");
}

_Noreturn void abort(void) {
    gk_mpy_fatal("abort");
}

#ifndef NDEBUG
void __assert_func(const char *file, int line, const char *func, const char *expr) {
    (void)file, (void)line, (void)func;
    gk_mpy_fatal(expr);
}
#endif
