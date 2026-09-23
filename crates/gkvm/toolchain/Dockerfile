# gk-toolchain — everything a gkvm guest build needs, prebuilt: the rv64 cross compiler +
# picolibc headers (C guests, gk-guest-crt) and MicroPython at the pinned commit with
# mpy-cross already built (Python guests). `gk build` (solidity-sdk tools/gk) uses this image
# when it can pull it and falls back to `ubuntu:24.04` + apt-get otherwise; the two produce the
# same bytes — the toolchain packages are the same Ubuntu 24.04 ones.
#
# Pins must match solidity-sdk tools/gk/gk_python.py and crates/gkvm/guest/micropython/Makefile.
FROM ubuntu:24.04
ARG MPY_TAG=v1.29.0
ARG MPY_COMMIT=0fd6c573ea815774668bbb16b8e197c8822368b2
ARG MPY_LIB_COMMIT=ee4bb8ff139e24c42b739935fbd8ec7c4d061e02
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
        make gcc libc6-dev python3 git ca-certificates \
        gcc-riscv64-unknown-elf picolibc-riscv64-unknown-elf \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch "$MPY_TAG" https://github.com/micropython/micropython /opt/micropython \
    && cd /opt/micropython \
    && git submodule update --init --depth 1 lib/micropython-lib \
    && test "$(git rev-parse HEAD)" = "$MPY_COMMIT" \
    && test "$(git -C lib/micropython-lib rev-parse HEAD)" = "$MPY_LIB_COMMIT" \
    && make -C mpy-cross -j"$(nproc)" >/dev/null \
    && test -x mpy-cross/build/mpy-cross
LABEL org.opencontainers.image.source="https://github.com/gas-killer/gas-analyzer" \
      org.opencontainers.image.description="gkvm guest toolchain: riscv64-unknown-elf-gcc + picolibc + MicroPython ${MPY_TAG} (mpy-cross prebuilt)" \
      gaskiller.gkvm.micropython.commit="${MPY_COMMIT}"
WORKDIR /build
