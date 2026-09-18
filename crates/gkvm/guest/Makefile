# gkvm guest builds.
#
#   make c            hello.c + bench.c + artifact-probe.c via riscv64-unknown-elf-gcc
#   make rust         hello-rs via the succinct rustup toolchain
#   make docker-c     the C targets inside ubuntu:24.04 (for hosts without the cross gcc)
#   make fixtures     copy built guests into ../tests/fixtures/
#   make qwen         the flagship guest + its logits test twin (byte-reproducible recipe)
#   make docker-qwen  `make qwen` inside ubuntu:24.04
#   make fixtures-qwen  copy the two qwen ELFs into ../tests/fixtures/
#
# All guests share link.ld and the entry/W^X/layout rules it encodes; see
# link.ld for the SP1 v6 loader contract.

CC := riscv64-unknown-elf-gcc
CFLAGS := -march=rv64im -mabi=lp64 -mcmodel=medany -O2 -Wall -Wextra \
          -ffreestanding -fno-builtin -fno-common -fno-jump-tables -nostdlib -nostartfiles \
          -static -T link.ld -Wl,--build-id=none
CRT := crt/crt0.S crt/gkvm.c

BUILD := build

c: $(BUILD)/hello-c.elf $(BUILD)/bench-c.elf $(BUILD)/artifact-probe-c.elf

$(BUILD)/hello-c.elf: hello/hello.c $(CRT) crt/gkvm.h link.ld
	@mkdir -p $(BUILD)
	$(CC) $(CFLAGS) $(CRT) hello/hello.c -o $@

$(BUILD)/bench-c.elf: bench/bench.c $(CRT) crt/gkvm.h link.ld
	@mkdir -p $(BUILD)
	$(CC) $(CFLAGS) $(CRT) bench/bench.c -o $@

$(BUILD)/artifact-probe-c.elf: artifact-probe/artifact-probe.c $(CRT) crt/gkvm.h link.ld
	@mkdir -p $(BUILD)
	$(CC) $(CFLAGS) $(CRT) artifact-probe/artifact-probe.c -o $@

# qwen: each unit is compiled to a NAMED object and then linked. The one-shot
# `gcc a.S b.c -o x.elf` recipe above assembles into a random /tmp/ccXXXXXX.o
# whose name lands in .symtab, so its ELF bytes — and therefore programHash —
# differ from build to build; this one is a function of the sources.
CFLAGS_OBJ := -march=rv64im -mabi=lp64 -mcmodel=medany -O2 -Wall -Wextra \
              -ffreestanding -fno-builtin -fno-common -fno-jump-tables
LDFLAGS_ELF := -march=rv64im -mabi=lp64 -nostdlib -nostartfiles -static -T link.ld -Wl,--build-id=none

qwen: $(BUILD)/qwen-c.elf $(BUILD)/qwen-logits-c.elf

$(BUILD)/qwen/crt0.o: crt/crt0.S
	@mkdir -p $(BUILD)/qwen
	$(CC) $(CFLAGS_OBJ) -c crt/crt0.S -o $@

$(BUILD)/qwen/gkvm.o: crt/gkvm.c crt/gkvm.h
	@mkdir -p $(BUILD)/qwen
	$(CC) $(CFLAGS_OBJ) -c crt/gkvm.c -o $@

$(BUILD)/qwen/qwen.o: qwen/qwen.c crt/gkvm.h
	@mkdir -p $(BUILD)/qwen
	$(CC) $(CFLAGS_OBJ) -c qwen/qwen.c -o $@

$(BUILD)/qwen/qwen-logits.o: qwen/qwen.c crt/gkvm.h
	@mkdir -p $(BUILD)/qwen
	$(CC) $(CFLAGS_OBJ) -DQWEN_EMIT_LOGITS -c qwen/qwen.c -o $@

$(BUILD)/qwen-c.elf: $(BUILD)/qwen/crt0.o $(BUILD)/qwen/gkvm.o $(BUILD)/qwen/qwen.o link.ld
	$(CC) $(LDFLAGS_ELF) $(BUILD)/qwen/crt0.o $(BUILD)/qwen/gkvm.o $(BUILD)/qwen/qwen.o -o $@

$(BUILD)/qwen-logits-c.elf: $(BUILD)/qwen/crt0.o $(BUILD)/qwen/gkvm.o $(BUILD)/qwen/qwen-logits.o link.ld
	$(CC) $(LDFLAGS_ELF) $(BUILD)/qwen/crt0.o $(BUILD)/qwen/gkvm.o $(BUILD)/qwen/qwen-logits.o -o $@

docker-qwen:
	docker run --rm -v "$(CURDIR)":/guest -w /guest ubuntu:24.04 bash -c \
	  "apt-get update -qq >/dev/null && apt-get install -y -qq make gcc-riscv64-unknown-elf >/dev/null && make qwen"

fixtures-qwen:
	mkdir -p ../tests/fixtures
	cp $(BUILD)/qwen-c.elf $(BUILD)/qwen-logits-c.elf ../tests/fixtures/

rust:
	cd hello-rs && cargo +succinct build --release --target riscv64im-succinct-zkvm-elf

docker-c:
	docker run --rm -v "$(CURDIR)":/guest -w /guest ubuntu:24.04 bash -c \
	  "apt-get update -qq >/dev/null && apt-get install -y -qq make gcc-riscv64-unknown-elf >/dev/null && make c"

fixtures:
	mkdir -p ../tests/fixtures
	cp $(BUILD)/hello-c.elf $(BUILD)/bench-c.elf $(BUILD)/artifact-probe-c.elf ../tests/fixtures/ 2>/dev/null || true
	cp hello-rs/target/riscv64im-succinct-zkvm-elf/release/hello-rs ../tests/fixtures/hello-rs.elf 2>/dev/null || true

clean:
	rm -rf $(BUILD)

.PHONY: c rust docker-c fixtures qwen docker-qwen fixtures-qwen clean
