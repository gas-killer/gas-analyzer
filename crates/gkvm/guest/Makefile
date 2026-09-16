# gkvm guest builds.
#
#   make c            hello.c + bench.c + artifact-probe.c via riscv64-unknown-elf-gcc
#   make rust         hello-rs via the succinct rustup toolchain
#   make docker-c     the C targets inside ubuntu:24.04 (for hosts without the cross gcc)
#   make fixtures     copy built guests into ../tests/fixtures/
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

.PHONY: c rust docker-c fixtures clean
