# hello: the Python twin of guest/hello/hello.c — a fixed tag followed by the
# payload reversed. Same bytes out as hello-c.elf and hello-rs.elf, so one
# expected answer checks the third toolchain too.
import gkvm

payload = gkvm.input()
gkvm.output(b"GKVM-HELLO-V1\n")
gkvm.output(bytes(reversed(payload)))
