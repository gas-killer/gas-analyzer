# Protocol reports

One-page Gas Killer savings reports, one per protocol. Every report uses the same template and
covers real mainnet transactions measured with this repository's analyzer. Savings assume the
Schnorr signature scheme.

| protocol | what saves | saving | report |
|---|---|---:|---|
| Securitize | BUIDL `transfer` (compliance service) | up to 88% | [securitize.md](securitize.md) |
| Kelp | rsETH `depositETH` | 81–83% | [kelp.md](kelp.md) |
| Railgun | private transfers, shields, RelayAdapt | 45–79% | [railgun.md](railgun.md) |
| Starknet | `updateStateKzgDA` | 54–75% | [starknet.md](starknet.md) |
| Polygon Agglayer | proof verification for X Layer, Katana, Lumia, Silicon | 54–72% | [polygon-agglayer.md](polygon-agglayer.md) |
| Derive | V3 proof submissions | 35–65% | [derive.md](derive.md) |
| Usual | USD0 `swap` and `redeem` (collateral check in Usd0) | 54–60% | [usual.md](usual.md) |

The underlying measurements are in `../ALL_TRANSACTION_ANALYSES.md`, `../PROTOCOL_SURVEY.md` and
`../NESTED_SETTLEMENT_RESULTS.md`. The Securitize, Usual and Railgun RelayAdapt figures use nested settlement (`--owned`), re-measured
on `main` at `8aed5cf` on 2026-10-13. All other figures were re-checked on the same build and did
not change.
