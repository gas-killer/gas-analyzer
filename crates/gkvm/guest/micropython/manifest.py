# Freeze exactly what the Makefile staged, in the order port.mk wrote into the
# generated manifest (GUEST_PY_EXTRA, then the guest script) — never the stage
# as a directory: that order would be the host filesystem's, and the freeze
# order is part of programHash. Nothing from micropython-lib is pulled in — a
# guest that wants a library module stages and freezes it explicitly.
include("$(GUEST_MANIFEST)")
