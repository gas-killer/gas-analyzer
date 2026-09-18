# setCode-drop measurement (UNBOUNDED_V3 M3): what the e2e saves by no longer
# mounting the model on the chain anvil.
#
#   make v3       time ArtifactMountV3::from_files over the artifacts (release)
#   make v2       time the e2e's deploy_anvil.py --overlay against anvil (docker)
#   make bench    both, v3 first (it generates the stand-in when none is given)
#
#   ARTIFACTS=dir   weights.bin + tokenizer.bin + vectors.json; defaults to a
#                   SYNTHETIC size-identical stand-in under target/ (24,299
#                   overlay chunks, as Qwen3-0.6B). Point it at the real
#                   qwen3-0.6b-onchain-v1 assets for the real-weights number.
#   SDK=dir         solidity-sdk checkout providing deploy_anvil.py
#   SAMPLES=n       samples per leg (default 3)
#
# Each leg prints one JSON line per sample (`wall_nanos`).

HERE := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
ROOT := $(abspath $(HERE)/../../../..)
ARTIFACTS ?= $(ROOT)/target/gkvm-setcode-bench/artifacts
SDK ?= $(ROOT)/../solidity-sdk
DEPLOY := $(SDK)/src/examples/onchain-llm/tools/deploy_anvil.py
SAMPLES ?= 3
IMAGE := gkvm-setcode-bench:foundry-v1.5.1

bench: v3 v2

v3:
	cd $(ROOT) && GK_BENCH_ARTIFACTS="$(abspath $(ARTIFACTS))" GK_BENCH_SAMPLES=$(SAMPLES) \
	  cargo test --release -p gas-analyzer-gkvm --test artifact_mount_bench -- --ignored --nocapture

image:
	docker build -q -t $(IMAGE) $(HERE)

v2: image
	docker run --rm --memory 8g -e SAMPLES=$(SAMPLES) \
	  -v "$(abspath $(ARTIFACTS))":/artifacts:ro \
	  -v "$(abspath $(DEPLOY))":/bench/deploy_anvil.py:ro \
	  $(IMAGE)

.PHONY: bench v3 v2 image
