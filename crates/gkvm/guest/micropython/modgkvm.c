/* `import gkvm` — GKVM_HOSTCALLS_V1 as Python sees it. A thin face over
 * gk-guest-crt: nothing here adds a capability the C header does not have.
 *
 *   gkvm.input() -> bytes                  the gkExec payload (any number of times)
 *   gkvm.output(buf)                       append to the guest result
 *   gkvm.abort(code, msg=b"")              deterministic trap, never returns
 *   gkvm.artifact_root() -> bytes          the 32-byte committed root
 *   gkvm.artifact_len(kind) -> int
 *   gkvm.artifact_read(kind, page) -> bytes            one verified 4,096-byte page
 *   gkvm.artifact_readinto(kind, page, buf)            same, into buf[:4096]
 *   gkvm.keccak256(buf) -> bytes
 *   gkvm.PAGE_SIZE
 *
 * Range errors on artifact access are the crt's traps, exactly as for a C
 * guest — they are not turned into Python exceptions, so a script cannot
 * catch its way past a bad page.
 */
#include "py/obj.h"
#include "py/runtime.h"

#include "gkport.h"

static mp_obj_t gkvm_input(void) {
    return mp_obj_new_bytes(gk_mpy_payload, gk_mpy_payload_len);
}
static MP_DEFINE_CONST_FUN_OBJ_0(gkvm_input_obj, gkvm_input);

static mp_obj_t gkvm_output(mp_obj_t buf_in) {
    mp_buffer_info_t buf;
    mp_get_buffer_raise(buf_in, &buf, MP_BUFFER_READ);
    gk_output_write(buf.buf, buf.len);
    return mp_const_none;
}
static MP_DEFINE_CONST_FUN_OBJ_1(gkvm_output_obj, gkvm_output);

static mp_obj_t gkvm_abort(size_t n_args, const mp_obj_t *args) {
    mp_int_t code = mp_obj_get_int(args[0]);
    if (code < 0 || code >= (mp_int_t)GK_MPY_TRAP_RESERVED_FROM) {
        mp_raise_ValueError(MP_ERROR_TEXT("trap code must be in 0..0xCFFFFFFF"));
    }
    mp_buffer_info_t msg = {0};
    if (n_args > 1) {
        mp_get_buffer_raise(args[1], &msg, MP_BUFFER_READ);
        if (msg.len > GK_MPY_TRAP_MSG_CAP) {
            msg.len = GK_MPY_TRAP_MSG_CAP;
        }
    }
    gk_abort((u32)code, msg.buf, (u32)msg.len);
}
static MP_DEFINE_CONST_FUN_OBJ_VAR_BETWEEN(gkvm_abort_obj, 1, 2, gkvm_abort);

static mp_obj_t gkvm_artifact_root(void) {
    return mp_obj_new_bytes(gk_artifact_root(), 32);
}
static MP_DEFINE_CONST_FUN_OBJ_0(gkvm_artifact_root_obj, gkvm_artifact_root);

static mp_obj_t gkvm_artifact_len(mp_obj_t kind_in) {
    return mp_obj_new_int_from_ull(gk_artifact_len((u32)mp_obj_get_int(kind_in)));
}
static MP_DEFINE_CONST_FUN_OBJ_1(gkvm_artifact_len_obj, gkvm_artifact_len);

static mp_obj_t gkvm_artifact_read(mp_obj_t kind_in, mp_obj_t page_in) {
    vstr_t page;
    vstr_init_len(&page, GK_ARTIFACT_PAGE_SIZE);
    gk_artifact_read((u32)mp_obj_get_int(kind_in), (u64)mp_obj_get_int(page_in), (u8 *)page.buf);
    return mp_obj_new_bytes_from_vstr(&page);
}
static MP_DEFINE_CONST_FUN_OBJ_2(gkvm_artifact_read_obj, gkvm_artifact_read);

static mp_obj_t gkvm_artifact_readinto(mp_obj_t kind_in, mp_obj_t page_in, mp_obj_t buf_in) {
    mp_buffer_info_t buf;
    mp_get_buffer_raise(buf_in, &buf, MP_BUFFER_WRITE);
    if (buf.len < GK_ARTIFACT_PAGE_SIZE) {
        mp_raise_ValueError(MP_ERROR_TEXT("buffer smaller than a page"));
    }
    gk_artifact_read((u32)mp_obj_get_int(kind_in), (u64)mp_obj_get_int(page_in), buf.buf);
    return mp_const_none;
}
static MP_DEFINE_CONST_FUN_OBJ_3(gkvm_artifact_readinto_obj, gkvm_artifact_readinto);

static mp_obj_t gkvm_keccak256(mp_obj_t buf_in) {
    mp_buffer_info_t buf;
    mp_get_buffer_raise(buf_in, &buf, MP_BUFFER_READ);
    u8 digest[32];
    gk_keccak256(buf.buf, buf.len, digest);
    return mp_obj_new_bytes(digest, sizeof digest);
}
static MP_DEFINE_CONST_FUN_OBJ_1(gkvm_keccak256_obj, gkvm_keccak256);

static const mp_rom_map_elem_t gkvm_module_globals_table[] = {
    { MP_ROM_QSTR(MP_QSTR___name__), MP_ROM_QSTR(MP_QSTR_gkvm) },
    { MP_ROM_QSTR(MP_QSTR_input), MP_ROM_PTR(&gkvm_input_obj) },
    { MP_ROM_QSTR(MP_QSTR_output), MP_ROM_PTR(&gkvm_output_obj) },
    { MP_ROM_QSTR(MP_QSTR_abort), MP_ROM_PTR(&gkvm_abort_obj) },
    { MP_ROM_QSTR(MP_QSTR_artifact_root), MP_ROM_PTR(&gkvm_artifact_root_obj) },
    { MP_ROM_QSTR(MP_QSTR_artifact_len), MP_ROM_PTR(&gkvm_artifact_len_obj) },
    { MP_ROM_QSTR(MP_QSTR_artifact_read), MP_ROM_PTR(&gkvm_artifact_read_obj) },
    { MP_ROM_QSTR(MP_QSTR_artifact_readinto), MP_ROM_PTR(&gkvm_artifact_readinto_obj) },
    { MP_ROM_QSTR(MP_QSTR_keccak256), MP_ROM_PTR(&gkvm_keccak256_obj) },
    { MP_ROM_QSTR(MP_QSTR_PAGE_SIZE), MP_ROM_INT(GK_ARTIFACT_PAGE_SIZE) },
};
static MP_DEFINE_CONST_DICT(gkvm_module_globals, gkvm_module_globals_table);

const mp_obj_module_t gkvm_module = {
    .base = { &mp_type_module },
    .globals = (mp_obj_dict_t *)&gkvm_module_globals,
};

MP_REGISTER_MODULE(MP_QSTR_gkvm, gkvm_module);
