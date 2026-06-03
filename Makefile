.PHONY: bench bench-rpc bench-save-baseline bench-compare flamegraph flamegraph-heap flamegraph-online flamegraph-speedscope flamegraph-speedscope-online fixture replay-fixture

BASELINE ?= main
# Fall back to .env for RPC_URL if not already set in the environment.
RPC_URL ?= $(shell [ -f .env ] && grep -m1 '^RPC_URL=' .env | cut -d= -f2-)

# Guard used by targets that require a live Sepolia node.
require-rpc-url:
	@[ -n "$(RPC_URL)" ] || { echo "Error: RPC_URL is not set. Pass it on the command line or add it to .env."; exit 1; }

# Run the offline benchmarks (no RPC required).
bench:
	cargo bench -p gas-analyzer-evmsketch --bench trace_parsing
	cargo bench -p gas-analyzer-evmsketch --bench gas_estimation
	cargo bench -p gas-analyzer-evmsketch --bench replay

# Run the end-to-end RPC benchmark.  Requires a live Sepolia node.
# Example:  make bench-rpc RPC_URL=https://rpc.sepolia.org
bench-rpc: require-rpc-url
	RPC_URL=$(RPC_URL) cargo bench -p gas-analyzer-evmsketch --bench end_to_end

# Save a named baseline for later comparison (default: BASELINE=main).
# Run this on the base branch before switching to a feature branch.
# Example:  make bench-save-baseline
#           make bench-save-baseline BASELINE=before-optimisation
bench-save-baseline:
	cargo bench -p gas-analyzer-evmsketch --bench trace_parsing -- --save-baseline $(BASELINE)
	cargo bench -p gas-analyzer-evmsketch --bench gas_estimation -- --save-baseline $(BASELINE)
	cargo bench -p gas-analyzer-evmsketch --bench replay -- --save-baseline $(BASELINE)

# Compare current results against a saved baseline (default: BASELINE=main).
# Example:  make bench-compare
#           make bench-compare BASELINE=before-optimisation
bench-compare:
	cargo bench -p gas-analyzer-evmsketch --bench trace_parsing -- --baseline $(BASELINE)
	cargo bench -p gas-analyzer-evmsketch --bench gas_estimation -- --baseline $(BASELINE)
	cargo bench -p gas-analyzer-evmsketch --bench replay -- --baseline $(BASELINE)

# CPU flamegraph for the offline critical paths (no RPC required).
# SVGs written to target/criterion/<group>/<bench>/profile/flamegraph.svg
# Example:  make flamegraph
flamegraph:
	RUSTFLAGS="-C force-frame-pointers=yes" cargo bench -p gas-analyzer-evmsketch --bench flamegraph -- --profile-time 10

# Heap profile for the offline critical paths.
# Output written to dhat-heap.json — open at https://nnethercote.github.io/dh_view/dh_view.html
# Example:  make flamegraph-heap
flamegraph-heap:
	cargo bench -p gas-analyzer-evmsketch --bench flamegraph --features gas-analyzer-evmsketch/heap-profile -- --profile-time 5

# CPU flamegraph including the full end-to-end RPC pipeline.
# Example:  make flamegraph-online RPC_URL=https://rpc.sepolia.org
flamegraph-online: require-rpc-url
	RPC_URL=$(RPC_URL) RUSTFLAGS="-C force-frame-pointers=yes" \
		cargo bench -p gas-analyzer-evmsketch --bench flamegraph -- --profile-time 60

# Speedscope protobuf output (offline) — import .pb files into https://speedscope.app
# Files written to target/criterion/<group>/<bench>/profile/profile.pb
# Example:  make flamegraph-speedscope
flamegraph-speedscope:
	FLAMEGRAPH_PROTO=1 RUSTFLAGS="-C force-frame-pointers=yes" \
		cargo bench -p gas-analyzer-evmsketch --bench flamegraph -- --profile-time 10

# Speedscope protobuf output including the full end-to-end RPC pipeline.
# Example:  make flamegraph-speedscope-online RPC_URL=https://rpc.sepolia.org
flamegraph-speedscope-online: require-rpc-url
	RPC_URL=$(RPC_URL) FLAMEGRAPH_PROTO=1 RUSTFLAGS="-C force-frame-pointers=yes" \
		cargo bench -p gas-analyzer-evmsketch --bench flamegraph -- --profile-time 60

# Regenerate the Sepolia trace fixture and commit it to LFS.
# Only needed when repinning the benchmark transaction.
# Example:  make fixture RPC_URL=https://rpc.sepolia.org
fixture: require-rpc-url
	RPC_URL=$(RPC_URL) cargo run -p gas-analyzer-evmsketch --example generate_fixture

# Regenerate the replay fixtures (preceding_txs.json + pre_block_state.json) and commit to LFS.
# Only needed when repinning the benchmark transaction.
# Example:  make replay-fixture RPC_URL=https://rpc.sepolia.org
replay-fixture: require-rpc-url
	RPC_URL=$(RPC_URL) cargo run -p gas-analyzer-evmsketch --example generate_replay_fixture
