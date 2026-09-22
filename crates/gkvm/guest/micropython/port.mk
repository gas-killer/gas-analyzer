# The MicroPython gkvm port proper: upstream's py/ + extmod/ build machinery,
# this directory's port files, gk-guest-crt, and ../link.ld. Driven by
# ./Makefile (which pins and fetches upstream); run directly only with
# MPY_SRC set and a riscv64-unknown-elf toolchain on PATH.
#
#   make -f port.mk MPY_SRC=<micropython checkout> [GUEST_PY=hello.py] [GUEST_PY_EXTRA="a.py b.py"]
#
# One guest script per image: it is frozen (mpy-cross bytecode linked into
# the ELF), so programHash commits to the interpreter AND the script.
# GUEST_PY_EXTRA modules are frozen next to it and importable by file stem
# (the sdk's `gk build` stages the user's module + gk_runtime.py this way).

ifeq ($(MPY_SRC),)
$(error MPY_SRC must point at the pinned MicroPython checkout — use ./Makefile)
endif

GUEST_PY ?= hello.py
ifeq ($(wildcard $(GUEST_PY)),)
$(error guest script not found: $(GUEST_PY))
endif
GUEST_NAME := $(basename $(notdir $(GUEST_PY)))

BUILD ?= ../build/micropython/$(GUEST_NAME)
ELF := $(BUILD)/$(GUEST_NAME)-py.elf

# Frozen-heap sizing. Both are baked into the image; the heap is NOBITS (free
# in the ELF, zeroed by the executor), but gc_init and every collection walk
# its allocation table, so bigger is not free in cycles.
GK_MPY_HEAP_BYTES ?= 16777216
GK_MPY_STACK_BYTES ?= 1048576

include $(MPY_SRC)/py/mkenv.mk

# programHash is keccak of the ELF, so the image must be a function of the
# sources. Upstream's py/makeversionhdr.py stamps sys.version with today's
# date and, when git can see the checkout, a describe/short-hash string whose
# length depends on the git version and clone depth. Pin the date to the
# v1.29.0 commit's and hide the repo from git, which drops it to the
# mpconfig.h fallback ("v1.29.0", "<no hash>") on every host.
export SOURCE_DATE_EPOCH := 1787579045
export GIT_DIR := /nonexistent-gkvm-reproducible-build

CROSS_COMPILE = riscv64-unknown-elf-
QSTR_DEFS = qstrdefsport.h
MICROPY_ROM_TEXT_COMPRESSION ?= 1

# The stage holds the guest script (+ GUEST_PY_EXTRA) and nothing else. Staged
# at parse time: py/manifest.mk already evaluates the manifest while make is
# still reading makefiles.
#
# The freeze ORDER is part of the image, hence of programHash. Freezing the
# stage as a directory leaves the order to os.walk, i.e. to the host
# filesystem's readdir order (the same sources hashed differently on a CI
# runner). manifest.py therefore includes a generated manifest that names every
# script explicitly: GUEST_PY_EXTRA in the order given, then GUEST_PY.
GUEST_PY_EXTRA ?=
GUEST_STAGE := $(abspath $(BUILD)/stage)
GUEST_MANIFEST := $(abspath $(BUILD)/guest_manifest.py)
GUEST_FROZEN := $(notdir $(GUEST_PY_EXTRA) $(GUEST_PY))
$(shell rm -rf "$(GUEST_STAGE)" && mkdir -p "$(GUEST_STAGE)" && cp "$(GUEST_PY)" $(GUEST_PY_EXTRA) "$(GUEST_STAGE)/")
$(shell printf 'freeze_as_mpy("$$(GUEST_STAGE)", (%s))\n' '$(foreach f,$(GUEST_FROZEN),"$(f)",)' > "$(GUEST_MANIFEST)")
FROZEN_MANIFEST = manifest.py
MICROPY_MANIFEST_GUEST_STAGE = $(GUEST_STAGE)
MICROPY_MANIFEST_GUEST_MANIFEST = $(GUEST_MANIFEST)

include $(TOP)/py/py.mk
include $(TOP)/extmod/extmod.mk

# rv64im and nothing else: the SP1 v6 loader transpiles .text word by word, so
# no C (compressed), no A, no F/D, no Zicsr.
GK_ARCH = -march=rv64im -mabi=lp64
AFLAGS += $(GK_ARCH)

# Headers only — nothing is linked from the toolchain's libc (see gk_libc.c).
PICOLIBC_INC ?= /usr/lib/picolibc/riscv64-unknown-elf/include

INC += -I. -I$(TOP) -I$(BUILD)
CFLAGS += $(INC) -isystem $(PICOLIBC_INC) $(GK_ARCH) -mcmodel=medany \
          -std=gnu99 -Wall -Werror -O2 -DNDEBUG \
          -ffreestanding -fno-common -ffunction-sections -fdata-sections \
          -DGK_MPY_ENTRY='"$(notdir $(GUEST_PY))"' \
          -DGK_MPY_HEAP_BYTES=$(GK_MPY_HEAP_BYTES) \
          -DGK_MPY_STACK_BYTES=$(GK_MPY_STACK_BYTES)

SRC_C = \
	main.c \
	modgkvm.c \
	gk_libc.c \
	shared/runtime/gchelper_native.c \
	shared/runtime/stdout_helpers.c \

SRC_QSTR += main.c modgkvm.c

# gk-guest-crt, compiled with the flags every C guest uses (../Makefile's
# CFLAGS_OBJ), so a page read costs a Python guest what it costs a C one.
CRT_CFLAGS = $(GK_ARCH) -mcmodel=medany -O2 -Wall -Wextra \
             -ffreestanding -fno-builtin -fno-common -fno-jump-tables
CRT_OBJ = $(BUILD)/crt/crt0.o $(BUILD)/crt/gkvm.o

OBJ += $(PY_O)
OBJ += $(addprefix $(BUILD)/, $(SRC_C:.c=.o))
OBJ += $(BUILD)/shared/runtime/gchelper_rv64i.o
OBJ += $(CRT_OBJ)

all: $(ELF)

# make does not see -D changes: without this a resized heap would silently
# keep the old main.o. The stamp is rewritten only when the values move.
GK_CONFIG := $(GK_MPY_HEAP_BYTES) $(GK_MPY_STACK_BYTES) $(notdir $(GUEST_PY))
GK_CONFIG_STAMP := $(BUILD)/gk-config.stamp
$(shell mkdir -p "$(BUILD)" && { echo "$(GK_CONFIG)" | cmp -s - "$(GK_CONFIG_STAMP)" || echo "$(GK_CONFIG)" > "$(GK_CONFIG_STAMP)"; })
$(BUILD)/main.o: $(GK_CONFIG_STAMP)

$(BUILD)/crt/crt0.o: ../crt/crt0.S
	$(Q)$(CC) $(CRT_CFLAGS) -c $< -o $@

$(BUILD)/crt/gkvm.o: ../crt/gkvm.c ../crt/gkvm.h
	$(Q)$(CC) $(CRT_CFLAGS) -c $< -o $@

GK_LDFLAGS = $(GK_ARCH) -nostdlib -nostartfiles -static -T ../link.ld \
             -Wl,--build-id=none -Wl,--gc-sections -Wl,-Map=$(ELF:.elf=.map)

$(ELF): $(OBJ) ../link.ld
	$(ECHO) "LINK $@"
	$(Q)$(CC) $(GK_LDFLAGS) $(OBJ) -lgcc -o $@
	$(Q)$(SIZE) $@

include $(TOP)/py/mkrules.mk
