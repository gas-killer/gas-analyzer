//! End-to-end benchmark for `call_to_encoded_state_updates_with_evmsketch`.
//!
//! Requires a live Sepolia node:
//!   make bench-rpc RPC_URL=<sepolia-node>
//!
//! Each iteration makes several RPC calls; wall-time is dominated by network
//! latency. Use a small sample_size (default: 10) and a long measurement_time.

use std::time::Duration;

use alloy::primitives::{FixedBytes, TxKind};
use alloy::providers::ProviderBuilder;
use alloy::rpc::types::eth::{TransactionInput, TransactionRequest};
use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use alloy_rpc_types::TransactionTrait;
use criterion::{Criterion, black_box, criterion_group, criterion_main};

const SEPOLIA_TX_HASH: &str = "0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb";

fn bench_end_to_end(c: &mut Criterion) {
    let rpc_url = match std::env::var("RPC_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!(
                "Skipping end_to_end bench: RPC_URL not set.\n\
                 Run `make bench-rpc RPC_URL=<sepolia-node>` to include this benchmark."
            );
            return;
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");

    // Fetch tx details once outside the measured region.
    let (tx_request, block) = rt.block_on(async {
        let provider =
            ProviderBuilder::new().connect_http(rpc_url.parse().expect("invalid RPC_URL"));

        let hash: FixedBytes<32> = SEPOLIA_TX_HASH.parse().expect("invalid tx hash constant");

        let tx = provider
            .get_transaction_by_hash(hash)
            .await
            .expect("RPC error on get_transaction_by_hash")
            .expect("tx not found — check RPC_URL points to Sepolia");

        let receipt = provider
            .get_transaction_receipt(hash)
            .await
            .expect("RPC error on get_transaction_receipt")
            .expect("receipt not found");

        let block_num = receipt.block_number.expect("receipt has no block_number");

        let req = TransactionRequest {
            from: Some(tx.inner.signer()),
            to: tx.inner.to().map(TxKind::Call),
            input: TransactionInput::new(tx.inner.input().clone()),
            value: Some(tx.inner.value()),
            ..Default::default()
        };

        (req, BlockNumberOrTag::Number(block_num))
    });

    eprintln!("end_to_end: pinned to block {block:?}, tx {SEPOLIA_TX_HASH}");

    let mut group = c.benchmark_group("end_to_end");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(150));

    group.bench_function("call_to_encoded_state_updates_with_evmsketch", |b| {
        b.to_async(&rt).iter(|| async {
            black_box(
                gas_analyzer_evmsketch::call_to_encoded_state_updates_with_evmsketch(
                    &rpc_url,
                    tx_request.clone(),
                    block,
                )
                .await
                .expect("end-to-end estimation failed"),
            )
        })
    });

    group.finish();
}

criterion_group!(benches, bench_end_to_end);
criterion_main!(benches);
