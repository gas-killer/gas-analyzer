/* Shared between the port's entry (main.c) and the `gkvm` module. */
#ifndef GKVM_MICROPYTHON_GKPORT_H
#define GKVM_MICROPYTHON_GKPORT_H

#include "../crt/gkvm.h"

/* Trap codes raised by the MicroPython runtime itself. gk-guest-crt owns
 * 0xE…, the host owns 0xF…; this port takes 0xD…, and gkvm.abort() refuses
 * every code from 0xD0000000 up so a script cannot forge any of the three. */
#define GK_MPY_TRAP_EXCEPTION 0xD0000001u   /* uncaught exception; msg = traceback */
#define GK_MPY_TRAP_SYSTEM_EXIT 0xD0000002u /* sys.exit(x) with a truthy x */
#define GK_MPY_TRAP_FATAL 0xD0000003u       /* interpreter-level failure */
#define GK_MPY_TRAP_RESERVED_FROM 0xD0000000u

/* Tracebacks are cut here; the frame rides the output stream and must leave
 * room under GK_OUTPUT_BYTES_CAP for whatever the script already wrote. */
#define GK_MPY_TRAP_MSG_CAP 1024u

extern u8 gk_mpy_payload[GK_INPUT_BYTES_CAP + 8];
extern u64 gk_mpy_payload_len;

_Noreturn void gk_mpy_fatal(const char *msg);

#endif
