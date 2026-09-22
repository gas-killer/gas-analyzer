# gkvm guest builds.
#
#   make c            hello.c + bench.c via riscv64-unknown-elf-gcc
#   make rust         hello-rs via the succinct rustup toolchain
#   make docker-c     the C targets inside ubuntu:24.04 (for hosts without the cross gcc)
#   make fixtures     copy built guests into ../tests/fixtures/
#   make qwen         the flagship guest + its logits test twin (byte-reproducible recipe)
#   make probe        artifact-probe.c (same recipe)
#   make docker-qwen  `make qwen probe` inside ubuntu:24.04
#   make fixtures-qwen  copy the two qwen ELFs + artifact-probe into ../tests/fixtures/
#
# Python guests are their own build (MicroPython pinned upstream, fetched into
# target/, frozen-bytecode images): `make -C micropython fetch docker
# [GUEST_PY=hello.py]` — see micropython/Makefile.
#
# All guests share link.ld and the entry/W^X/layout rules it encodes; see
# link.ld for the SP1 v6 loader contract.
#
# Every C guest is built with the same byte-reproducible recipe (named objects, then
# link), so a committed ELF is a function of the sources and the docker toolchain.

CC := riscv64-unknown-elf-gcc

BUILD := build

c: $(BUILD)/hello-c.elf $(BUILD)/bench-c.elf

# Each unit is compiled to a NAMED object and then linked. A one-shot
# `gcc a.S b.c -o x.elf` assembles into a random /tmp/ccXXXXXX.o whose name
# lands in .symtab, so its ELF bytes — and therefore programHash — differ from
# build to build; this recipe is a function of the sources.
CFLAGS_OBJ := -march=rv64im -mabi=lp64 -mcmodel=medany -O2 -Wall -Wextra \
              -ffreestanding -fno-builtin -fno-common -fno-jump-tables
LDFLAGS_ELF := -march=rv64im -mabi=lp64 -nostdlib -nostartfiles -static -T link.ld -Wl,--build-id=none
OBJ := $(BUILD)/obj
CRT_OBJ := $(OBJ)/crt0.o $(OBJ)/gkvm.o

qwen: $(BUILD)/qwen-c.elf $(BUILD)/qwen-logits-c.elf

probe: $(BUILD)/artifact-probe-c.elf

$(OBJ)/crt0.o: crt/crt0.S
	@mkdir -p $(OBJ)
	$(CC) $(CFLAGS_OBJ) -c crt/crt0.S -o $@

$(OBJ)/gkvm.o: crt/gkvm.c crt/gkvm.h
	@mkdir -p $(OBJ)
	$(CC) $(CFLAGS_OBJ) -c crt/gkvm.c -o $@

$(OBJ)/hello.o: hello/hello.c crt/gkvm.h
	@mkdir -p $(OBJ)
	$(CC) $(CFLAGS_OBJ) -c hello/hello.c -o $@

$(OBJ)/bench.o: bench/bench.c crt/gkvm.h
	@mkdir -p $(OBJ)
	$(CC) $(CFLAGS_OBJ) -c bench/bench.c -o $@

$(BUILD)/hello-c.elf: $(CRT_OBJ) $(OBJ)/hello.o link.ld
	$(CC) $(LDFLAGS_ELF) $(CRT_OBJ) $(OBJ)/hello.o -o $@

$(BUILD)/bench-c.elf: $(CRT_OBJ) $(OBJ)/bench.o link.ld
	$(CC) $(LDFLAGS_ELF) $(CRT_OBJ) $(OBJ)/bench.o -o $@

$(OBJ)/qwen.o: qwen/qwen.c crt/gkvm.h
	@mkdir -p $(OBJ)
	$(CC) $(CFLAGS_OBJ) -c qwen/qwen.c -o $@

$(OBJ)/qwen-logits.o: qwen/qwen.c crt/gkvm.h
	@mkdir -p $(OBJ)
	$(CC) $(CFLAGS_OBJ) -DQWEN_EMIT_LOGITS -c qwen/qwen.c -o $@

$(OBJ)/artifact-probe.o: artifact-probe/artifact-probe.c crt/gkvm.h
	@mkdir -p $(OBJ)
	$(CC) $(CFLAGS_OBJ) -c artifact-probe/artifact-probe.c -o $@

$(BUILD)/qwen-c.elf: $(CRT_OBJ) $(OBJ)/qwen.o link.ld
	$(CC) $(LDFLAGS_ELF) $(CRT_OBJ) $(OBJ)/qwen.o -o $@

$(BUILD)/qwen-logits-c.elf: $(CRT_OBJ) $(OBJ)/qwen-logits.o link.ld
	$(CC) $(LDFLAGS_ELF) $(CRT_OBJ) $(OBJ)/qwen-logits.o -o $@

$(BUILD)/artifact-probe-c.elf: $(CRT_OBJ) $(OBJ)/artifact-probe.o link.ld
	$(CC) $(LDFLAGS_ELF) $(CRT_OBJ) $(OBJ)/artifact-probe.o -o $@

docker-qwen:
	docker run --rm -v "$(CURDIR)":/guest -w /guest ubuntu:24.04 bash -c \
	  "apt-get update -qq >/dev/null && apt-get install -y -qq make gcc-riscv64-unknown-elf >/dev/null && make qwen probe"

fixtures-qwen:
	mkdir -p ../tests/fixtures
	cp $(BUILD)/qwen-c.elf $(BUILD)/qwen-logits-c.elf $(BUILD)/artifact-probe-c.elf ../tests/fixtures/

rust:
	cd hello-rs && cargo +succinct build --release --target riscv64im-succinct-zkvm-elf

docker-c:
	docker run --rm -v "$(CURDIR)":/guest -w /guest ubuntu:24.04 bash -c \
	  "apt-get update -qq >/dev/null && apt-get install -y -qq make gcc-riscv64-unknown-elf >/dev/null && make c"

fixtures:
	mkdir -p ../tests/fixtures
	cp $(BUILD)/hello-c.elf $(BUILD)/bench-c.elf ../tests/fixtures/ 2>/dev/null || true
	cp hello-rs/target/riscv64im-succinct-zkvm-elf/release/hello-rs ../tests/fixtures/hello-rs.elf 2>/dev/null || true

clean:
	rm -rf $(BUILD)

.PHONY: c rust docker-c fixtures qwen probe docker-qwen fixtures-qwen clean
