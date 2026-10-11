# Protocol reports

One-page Gas Killer savings reports, one per protocol. Every report uses the same template and
covers real mainnet transactions measured with this repository's analyzer. Savings assume the
Schnorr signature scheme.

| protocol | what saves | saving | report |
|---|---|---:|---|
| Securitize | BUIDL `transfer` (compliance service) | up to 88% | [securitize.md](securitize.md) |
| Kelp | rsETH `depositETH` | 81–83% | [kelp.md](kelp.md) |
| Railgun | private transfers, shields, RelayAdapt | 55–79% | [railgun.md](railgun.md) |
| Starknet | `updateStateKzgDA` | 54–75% | [starknet.md](starknet.md) |
| Polygon zkEVM | batch verification | 54–72% | [polygon-zkevm.md](polygon-zkevm.md) |
| Derive | V3 proof submissions | 35–65% | [derive.md](derive.md) |

The underlying measurements are in `../ALL_TRANSACTION_ANALYSES.md`, `../PROTOCOL_SURVEY.md` and
`../NESTED_SETTLEMENT_RESULTS.md`. The Securitize figures use the nested-settlement analyzer on
branch `nested-settlement`.
