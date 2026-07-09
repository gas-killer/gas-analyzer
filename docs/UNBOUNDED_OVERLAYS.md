# Pinned code overlays (`UNBOUNDED_V2`)

Extension of the unbounded simulation profile ([UNBOUNDED_MODE.md](./UNBOUNDED_MODE.md))
that mounts large immutable byte blobs — model weights, lookup tables, any read-only
data — as contract **code in the simulation environment**, without the blobs ever being
deployed on-chain.

Motivating consumer: the on-chain LLM in gas-killer/solidity-sdk
(`src/examples/onchain-llm/`). Qwen3-0.6B needs 597MB of int8 weights: ~24.3k data
contracts and ~130B gas to deploy (~$0.4–4.5M on mainnet) — or, with overlays, **one
32-byte hash** in the consumer's immutables.

## Mechanism

- **Manifest.** `manifestHash = keccak256(keccak256(weightsBlob) || keccak256(tokenizerBlob))`
  (`overlay_manifest_hash`). This is the consumer's entire on-chain commitment.
- **Chunking.** Blobs split into 24,575-byte chunks (`OVERLAY_CHUNK_PAYLOAD` = EIP-170
  minus the STOP prefix), globally indexed: weight chunks first, then tokenizer chunks.
  Each chunk mounts as runtime code `0x00 || payload` — byte-identical to the
  SSTORE2-style data contracts of directory mode, so consumer read offsets don't change.
- **Derived addresses.** Chunk `i` lives at
  `address(keccak256("gaskiller.llm.overlay.v1" || manifestHash || u64_be(i)))[12..]`
  (`overlay_chunk_address`, mirroring `Qwen3Engine.overlayChunkAddress` in solidity-sdk;
  cross-pinned by test vectors in `core/src/overlay.rs`). No on-chain directory needed.
- **Mounting.** [`OverlayEnv::from_blobs`] chunks, derives, and hashes;
  [`OverlayEnv::verify`] refuses bytes that don't reproduce the pinned manifest. The
  `*_with_env` RPC helpers and
  `call_to_encoded_state_updates_with_evmsketch_env` thread the bindings into
  `debug_traceCall` as `stateOverrides` `{address: {code}}` alongside the V1 gas
  overrides. To EVM execution an overlaid chunk is indistinguishable from deployed code
  (`EXTCODESIZE`/`EXTCODECOPY` identical).

## Determinism and slashing (read this twice)

Same bargain as V1's gas constants, extended: the overlay set is part of the
**versioned pinned environment**. [`env_commitment`] hashes
`domain || block_gas || tx_gas || manifest || n || (address, codeHash)*` — the V2
domain when overlays are present, V1 otherwise. **This commitment must be bound into
the SP1 slashing guest's `chainConfigHash`** (companion guest change, exactly like the
V1 env overrides): a fraud proof simulated under different or missing overlay bytes
then cannot verify, and honest operators using the pinned bytes cannot be falsely
slashed. Until the guest binding ships, overlay mode must not be used for slashable
quorums.

Notes:

- The shape gate is unchanged. Overlaid chunks are STOP-prefixed pure data: they cannot
  execute, cannot SSTORE, and payload `Store`s always target the consumer's own storage,
  so no additional gate rule is required.
- Payload gas estimation stays on the real chain env (unchanged from V1): overlaid code
  is only read during simulation and never appears in the payload.
- Operators fetch blobs out-of-band (HuggingFace/IPFS/mirrors) **once**; availability is
  a liveness concern, never a safety one — bytes are verified against the manifest
  before mounting.

## Operator lifecycle

1. Fetch `weights.bin` + `tokenizer.bin` for the consumer's pinned manifest.
2. `OverlayEnv::from_blobs(...)` then `env.verify(pinned_manifest)` — hard-refuse on
   mismatch.
3. Serve tracked calls via `call_to_encoded_state_updates_with_evmsketch_env(...,
   SimProfile::UnboundedV1, Some(&env))`.
4. Sanity check inside the mounted env: the consumer engine's
   `checkArtifacts(address(0), manifestHash, packedConfig)` view must pass.
