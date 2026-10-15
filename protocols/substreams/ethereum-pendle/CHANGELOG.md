# Changelog

## v0.1.0

Initial Pendle V2 integration on Ethereum, from the original market factory's deployment (block
16032059) and covering all five factories. Emits two component types under one protocol system:
`pendle_market`, the SY↔PT AMM, and `pendle_sy`, the ERC-5115 wrapper behind it.

Market reserves are replayed from `Swap`, `Mint` and `Burn`, which are identical across factory
generations where the storage layout is not. Fees are read back from each factory's
`getMarketConfig`. Each SY's tokens are classified once at creation as `one_to_one` or `index_rate`
by probing `previewDeposit` and `previewRedeem`; a token matching neither is left out.
`SY.exchangeRate()` and the PY index are read every `sy_rate_refresh_blocks` blocks (default 1),
and SY balances are read with `balanceOf` on any block a transfer moved one.
