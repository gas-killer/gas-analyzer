//! Differential-equality tests: every fixture must produce byte-identical
//! returndata + gas_used + success/revert/halt classification through the
//! interpreter and the revmc-compiled path. This is the EVM-exactness /
//! consensus property required to wire revmc into the local executor.

use revmc_harness::{
    Fixture, bench_compiled, bench_interpreted, extcodecopy, extcodecopy_loop, matmul, oog_fixture,
    revert_fixture, run_compiled, run_interpreted,
};

fn correctness_fixtures() -> Vec<Fixture> {
    vec![
        matmul(1_000, 5_000_000),         // packed-int arith loop -> Success
        extcodecopy(),                    // EXTCODECOPY 24575B + KECCAK -> Success
        extcodecopy_loop(50, 50_000_000), // EXTCODECOPY-in-loop + arith -> Success
        revert_fixture(),                 // REVERT with data
        oog_fixture(),                    // near-gas-cap: unbounded loop -> Halt(OutOfGas)
    ]
}

#[test]
fn differential_equality() {
    let mut failures = Vec::new();
    for fx in correctness_fixtures() {
        let i = run_interpreted(&fx);
        let c = run_compiled(&fx);

        if i != c {
            failures.push(format!(
                "[{}] DIVERGED\n    interpreted: {:?}\n    compiled:   {:?}",
                fx.name, i, c
            ));
        } else {
            eprintln!(
                "[OK] {:<12} class={:<26} tx_gas_used={:>12} out={}B",
                fx.name,
                i.class,
                i.tx_gas_used(),
                i.output.len()
            );
        }
    }
    assert!(failures.is_empty(), "interpreted vs compiled diverged:\n{}", failures.join("\n"));
}

fn report_speedup(label: &str, fx: &Fixture) {
    let (i, interp_dur) = bench_interpreted(fx);
    let (c, comp_dur) = bench_compiled(fx, true);

    // Differential equality must hold on the heavy timing run too.
    assert_eq!(i, c, "[{label}] interpreted vs compiled diverged on the heavy run");

    let ratio = interp_dur.as_secs_f64() / comp_dur.as_secs_f64();
    let ggas = |d: std::time::Duration| i.tx_gas_used() as f64 / d.as_secs_f64() / 1e9;
    eprintln!("\n=== {label} ===");
    eprintln!("tx_gas_used  : {}", i.tx_gas_used());
    eprintln!("interpreted  : {:?}  ({:.3} Ggas/s)", interp_dur, ggas(interp_dur));
    eprintln!("compiled     : {:?}  ({:.3} Ggas/s)", comp_dur, ggas(comp_dur));
    eprintln!("speedup      : {ratio:.2}x");
}

#[test]
fn speedup_report() {
    // Register-only arith loop: an UPPER-BOUND microbenchmark — the compiler
    // keeps the whole loop in registers with block-level gas metering, so the
    // ratio is large and NOT representative of the real workload.
    report_speedup("matmul  (register-only, upper bound)", &matmul(2_000_000, 50_000_000_000));

    // EXTCODECOPY-in-loop: memory-bandwidth / host-call bound, representative of
    // the engine's real "copy weight chunk + integer matmul" inner loop. This
    // is the number to compare against the ~3-8x expectation.
    report_speedup(
        "extcodecopy_loop (memory/host-bound, representative)",
        &extcodecopy_loop(20_000, 5_000_000_000),
    );
}
