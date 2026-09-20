# probe: exercises the parts of the port a hello cannot — the collector (stack
# and register scanning), big integers, NLR (caught exceptions, the C-stack
# limit), the frozen-module entry as __main__, and every way out of a guest.
#
# payload = one mode byte-string:
#   b""       run the checks, answer with a keccak over their results
#   b"raise"  uncaught exception     -> trap 0xD0000001, msg = traceback
#   b"exit"   sys.exit(3)            -> trap 0xD0000002
#   b"abort"  gkvm.abort(7, b"bye")  -> trap 7
#   b"forge"  gkvm.abort(0xE0000002) -> ValueError -> trap 0xD0000001
import gc
import struct
import sys

import gkvm


def churn(rounds):
    # ~64 MiB of short-lived garbage through a 16 MiB heap: only survives if
    # collections run and the live list (reachable from this frame) is kept.
    keep = []
    for i in range(rounds):
        block = bytearray(65536)
        block[0] = i & 0xFF
        if i % 100 == 0:
            keep.append(block)
    return sum(b[0] for b in keep), len(keep)


def depth(n):
    return 0 if n == 0 else 1 + depth(n - 1)


def checks():
    out = []
    out.append(__name__.encode())
    out.append(sys.platform.encode())
    out.append(str(3**200 % (2**61 - 1)).encode())
    out.append(str((1 << 300) // 7919).encode())
    out.append(struct.pack(">IQ", 0xDEADBEEF, 1 << 40))
    out.append(repr(churn(1000)).encode())
    try:
        1 // 0
    except ZeroDivisionError as e:
        out.append(type(e).__name__.encode())
    try:
        depth(1 << 20)
    except RuntimeError as e:
        out.append(str(e).encode())
    gc.collect()
    out.append(gkvm.keccak256(b""))
    out.append(gkvm.artifact_root())
    return out


def fail():
    raise ValueError("probe: uncaught on purpose")


mode = gkvm.input()
if mode == b"":
    results = checks()
    gkvm.output(b"GKVM-MPY-PROBE-V1\n")
    gkvm.output(gkvm.keccak256(b"\x00".join(results)))
    gkvm.output(b"\n".join(results[:3]))
elif mode == b"raise":
    gkvm.output(b"partial")
    fail()
elif mode == b"exit":
    sys.exit(3)
elif mode == b"abort":
    gkvm.abort(7, b"bye")
elif mode == b"forge":
    gkvm.abort(0xE0000002, b"not the crt")
else:
    gkvm.abort(1, b"unknown mode")
