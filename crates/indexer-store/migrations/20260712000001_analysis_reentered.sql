-- Re-entrancy flag (#113): a callee called back into the target contract
-- during the traced execution. The heuristic counts callback gas as
-- unoptimizable external gas, so flagged heuristic rows may understate
-- savings. Flag-only policy; the estimate itself is not corrected.
ALTER TABLE analysis
    ADD COLUMN reentered BOOLEAN NOT NULL DEFAULT FALSE;
