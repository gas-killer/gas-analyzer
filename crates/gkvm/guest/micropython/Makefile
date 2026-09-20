# MicroPython on gkvm — driver. The port itself is port.mk; this file pins
# upstream, fetches it, and runs the build where a toolchain exists.
#
#   make fetch        clone MicroPython at the pin into $(MPY_SRC) (gitignored target/)
#   make check-pin    fail unless $(MPY_SRC) is exactly the pinned commit, unmodified
#   make docker       build $(GUEST_PY) inside ubuntu:24.04 (no riscv gcc needed on the host)
#   make native       same, with the host's riscv64-unknown-elf-gcc + picolibc headers
#   make fixtures     copy the built hello-py.elf into ../../tests/fixtures/
#   make clean
#
# Output: ../build/micropython/<name>/<name>-py.elf
#
# Upstream is NOT vendored: a guest's programHash commits to the interpreter
# bytes, so what matters is that the source is pinned and the build recipe is
# fixed — the docker leg mounts the sources at fixed paths (/guest, /mpy).

MPY_REPO ?= https://github.com/micropython/micropython
MPY_TAG := v1.29.0
MPY_COMMIT := 0fd6c573ea815774668bbb16b8e197c8822368b2
# lib/micropython-lib at that tag; py/manifest.mk refuses to freeze without it.
MPY_LIB_COMMIT := ee4bb8ff139e24c42b739935fbd8ec7c4d061e02

MPY_SRC ?= $(abspath ../../../../target/gkvm-micropython/src)
GUEST_ROOT := $(abspath ..)
GUEST_PY ?= hello.py

PORT_ARGS = GUEST_PY=$(GUEST_PY) $(if $(GK_MPY_HEAP_BYTES),GK_MPY_HEAP_BYTES=$(GK_MPY_HEAP_BYTES)) \
            $(if $(GK_MPY_STACK_BYTES),GK_MPY_STACK_BYTES=$(GK_MPY_STACK_BYTES)) $(if $(V),V=$(V))

fetch:
	test -d "$(MPY_SRC)/.git" || git clone --depth 1 --branch $(MPY_TAG) $(MPY_REPO) "$(MPY_SRC)"
	git -C "$(MPY_SRC)" submodule update --init --depth 1 lib/micropython-lib
	$(MAKE) check-pin

check-pin:
	@test "$$(git -C "$(MPY_SRC)" rev-parse HEAD)" = "$(MPY_COMMIT)" || \
	  { echo "check-pin: $(MPY_SRC) is not MicroPython $(MPY_TAG) ($(MPY_COMMIT)) — run 'make fetch'"; exit 1; }
	@test "$$(git -C "$(MPY_SRC)/lib/micropython-lib" rev-parse HEAD)" = "$(MPY_LIB_COMMIT)" || \
	  { echo "check-pin: lib/micropython-lib is not at $(MPY_LIB_COMMIT)"; exit 1; }
	@test -z "$$(git -C "$(MPY_SRC)" status --porcelain --untracked-files=no)" || \
	  { echo "check-pin: $(MPY_SRC) has local modifications"; exit 1; }
	@echo "check-pin: MicroPython $(MPY_TAG) @ $(MPY_COMMIT), unmodified"

native: check-pin
	$(MAKE) -f port.mk MPY_SRC="$(MPY_SRC)" $(PORT_ARGS)

# mpy-cross is a host tool, hence the container's native gcc next to the
# cross one. Outputs are handed back to the owner of the mounted tree.
docker: check-pin
	docker run --rm -v "$(GUEST_ROOT)":/guest -v "$(MPY_SRC)":/mpy -w /guest/micropython ubuntu:24.04 bash -c \
	  "apt-get update -qq >/dev/null && apt-get install -y -qq make gcc libc6-dev python3 gcc-riscv64-unknown-elf picolibc-riscv64-unknown-elf >/dev/null && \
	   riscv64-unknown-elf-gcc --version | head -1 && \
	   make -f port.mk MPY_SRC=/mpy $(PORT_ARGS); rc=\$$?; \
	   chown -R \$$(stat -c %u:%g /guest/micropython) /guest/build/micropython /mpy/mpy-cross/build 2>/dev/null; exit \$$rc"

# Only hello.py is a committed fixture (tests/guest_e2e.rs pins its answer and
# its instruction count); probe.py is a hand-run smoke guest.
fixtures:
	mkdir -p ../../tests/fixtures
	cp ../build/micropython/hello/hello-py.elf ../../tests/fixtures/hello-py.elf

# Children only: ../build itself may be root-owned (left by a docker leg).
clean:
	rm -rf ../build/micropython/*

.PHONY: fetch check-pin native docker fixtures clean
