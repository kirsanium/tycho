use anyhow::Result;
use substreams::store::{StoreGet, StoreGetRaw};
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::prelude::*;

use crate::transfers::{component_transfers, is_market};

/// Tracks the ERC-20 balances the markets actually custody, from `Transfer` events.
///
/// These are deliberately not the market's `totalPt` / `totalSy` — those are reserves, they live
/// in `map_reserve_deltas`, and they differ, because `totalSy` excludes donated excess that
/// `skim()` later sweeps. The balance channel carries what the indexer can reconcile against
/// chain state, which is the real token balance of the component's own address.
///
/// The market custodies SY and PT; it holds no YT, and reports nothing for it, which the
/// reconciliation reads as zero — the market's on-chain YT balance. The market is also its own LP
/// token, so it emits `Transfer` from its own address: those are ignored because the LP token is
/// not in the market's token list.
///
/// SYs are left out. Their balances are read with `balanceOf` in `map_protocol_changes`, because
/// an SY can hold a rebasing token, and because it is usually created long after it first held
/// anything: replaying its transfers from the creation block would start from zero.
#[substreams::handlers::map]
pub fn map_relative_component_balance(
    block: eth::Block,
    store: StoreGetRaw,
) -> Result<BlockBalanceDeltas> {
    let mut balance_deltas = Vec::new();
    for transfer in component_transfers(&block, &store) {
        if !is_market(&store, &transfer.component_id) {
            continue;
        }
        balance_deltas.push(BalanceDelta {
            ord: transfer.log.ordinal,
            tx: Some(transfer.tx.into()),
            token: transfer.log.address.clone(),
            delta: transfer.amount.to_signed_bytes_be(),
            component_id: transfer.component_id.into_bytes(),
        });
    }
    Ok(BlockBalanceDeltas { balance_deltas })
}
