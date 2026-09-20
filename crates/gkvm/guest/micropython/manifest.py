# Freeze exactly what the Makefile staged: the guest script, alone in
# GUEST_STAGE (passed as a MICROPY_MANIFEST_* variable). Nothing from
# micropython-lib is pulled in — a guest that wants a library module stages
# and freezes it explicitly.
freeze_as_mpy("$(GUEST_STAGE)")
