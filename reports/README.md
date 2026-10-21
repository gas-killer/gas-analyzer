# Protocol reports

One-page Gas Killer savings reports, one per protocol. Every report uses the same template and
covers real mainnet transactions measured with this repository's analyzer. Savings assume the
Schnorr signature scheme.

| protocol | what saves | saving | report |
|---|---|---:|---|
| Securitize | BUIDL `transfer` (compliance service) | up to 88% | [securitize.md](securitize.md) |
| Kelp | rsETH `depositETH` | 81–83% | [kelp.md](kelp.md) |
| Railgun | private transfers, shields, RelayAdapt | 45–79% | [railgun.md](railgun.md) |
| Scroll | bundle finalisation (proof check) | 65% | [scroll.md](scroll.md) |
| Starknet | `updateStateKzgDA` — **corrected**: the saving was the proof and blob checks | ~2% or none | [starknet.md](starknet.md) |
| Polygon Agglayer | proof verification for X Layer, Katana, Lumia, Silicon | 54–72% | [polygon-agglayer.md](polygon-agglayer.md) |
| Aave | V3 `borrow` (health-factor price reads) | 14–64% | [aave.md](aave.md) |
| Alchemix | V3 `mint` and `burn` (Mix-Yield Token valuation) | 64–86% | [alchemix.md](alchemix.md) |
| Chronicle | Scribe `poke` (price updates) | 29–32% | [chronicle.md](chronicle.md) |
| Derive | V3 proof submissions | 35–65% | [derive.md](derive.md) |
| Usual | USD0 `swap` and `redeem` (collateral check in Usd0) | 54–60% | [usual.md](usual.md) |
| YieldNest | ynETH MAX `processAccounting` (strategy position reads) | 76–79% | [yieldnest.md](yieldnest.md) |
| Tacit | TacitEvmPool `transact` (Groth16 proof check) | 47–55% | [tacit.md](tacit.md) |

The underlying measurements are in `../ALL_TRANSACTION_ANALYSES.md`, `../PROTOCOL_SURVEY.md` and
`../NESTED_SETTLEMENT_RESULTS.md`. The Securitize, Usual and Railgun RelayAdapt figures use nested settlement (`--owned`), re-measured
on `main` at `8aed5cf` on 2026-10-13. All other figures were re-checked on the same build and did
not change.

> **L2 caveat (2026-10-21).** Almost all of the Scroll, Polygon Agglayer, Derive and Starknet savings
> is the on-chain proof check (or, for Starknet, the proof-fact and blob checks). Moving that off-chain
> would replace the validity proof with operator stake, which a rollup will not accept. Kept on-chain,
> Scroll, Derive and Starknet save nothing or about 2%, and Agglayer at most 7–14%. See
> `../PROTOCOL_SURVEY.md`, "L2 proof verification, revisited". The Starknet report is corrected; the
> others still show the original figures.
