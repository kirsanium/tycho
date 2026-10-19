//! Tokens that keep balances in shares: Lido's stETH and its forks (Lybra's eUSD), and xStocks
//! tokens such as STRCx.
//!
//! Such a token stores each holder's shares and derives the balance from a ratio that moves without
//! any `Transfer`: a rebase report, a fee, a multiplier update. A balance followed by `Transfer`
//! amounts therefore misses every move of the ratio, and with enough flow through the holder it
//! goes negative. These tokens also emit `TransferShares(from, to, shares)` next to every
//! `Transfer`, so a holder's shares can be followed exactly, and each such pair tells the
//! token's current amount per share. Both come from logs; nothing is read from the chain.

use itertools::Itertools;
use substreams::{
    hex,
    scalar::BigInt,
    store::{StoreGet, StoreGetString},
};
use substreams_ethereum::pb::eth::v2 as eth;

/// `TransferShares(address indexed from, address indexed to, uint256 sharesValue)`.
pub const TRANSFER_SHARES: [u8; 32] =
    hex!("9d9c909296d9c674451c0c24f02cb64981eb3b727f99865939192f880a755dcb");
/// `Transfer(address indexed from, address indexed to, uint256 value)`.
const TRANSFER: [u8; 32] = hex!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// Key under which `store_share_ratios` keeps a token's latest amount per share.
pub fn share_ratio_key(token: &[u8]) -> String {
    format!("share_ratio:0x{}", hex::encode(token))
}

/// Key under which `store_share_holders` lists the SYs whose balance of a token is kept in shares.
pub fn share_holders_key(token: &[u8]) -> String {
    format!("share_holders:0x{}", hex::encode(token))
}

/// Key under which `store_share_balances` keeps one component's shares of one token.
pub fn share_balance_key(component_id: &str, token: &[u8]) -> String {
    format!("shares:{component_id}:0x{}", hex::encode(token))
}

/// The SYs `store_share_holders` lists for `token`.
pub fn share_holders(holders: &StoreGetString, token: &[u8]) -> Vec<String> {
    holders
        .get_last(share_holders_key(token))
        .map(|listed| {
            listed
                .split(';')
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unique()
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `component_id` keeps its balance of `token` in shares.
pub fn is_share_holder(holders: &StoreGetString, component_id: &str, token: &[u8]) -> bool {
    share_holders(holders, token)
        .iter()
        .any(|holder| holder == component_id)
}

/// A token's amount per share, as the ratio of one `Transfer` amount to its `TransferShares`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareRatio {
    pub amount: BigInt,
    pub shares: BigInt,
}

impl ShareRatio {
    pub fn encode(&self) -> String {
        format!("{}:{}", self.amount, self.shares)
    }

    pub fn decode(value: &str) -> Option<Self> {
        let (amount, shares) = value.split_once(':')?;
        let ratio = Self { amount: amount.parse().ok()?, shares: shares.parse().ok()? };
        (ratio.amount > BigInt::zero() && ratio.shares > BigInt::zero()).then_some(ratio)
    }

    /// The balance `shares` is worth, rounded down as the tokens round it.
    pub fn to_amount(&self, shares: &BigInt) -> BigInt {
        shares.clone() * self.amount.clone() / self.shares.clone()
    }

    /// The shares a balance of `amount` holds, rounded down.
    pub fn to_shares(&self, amount: &BigInt) -> BigInt {
        amount.clone() * self.shares.clone() / self.amount.clone()
    }

    /// Whether `self` prices a share differently from `earlier` by more than one part in 1e12.
    ///
    /// Two transfers at the same ratio round differently, so their pairs disagree in the last few
    /// digits; that is not a move worth revaluing every holder for. A rebase moves the ratio by
    /// about 1e-4, a multiplier update by more.
    pub fn moved_from(&self, earlier: &ShareRatio) -> bool {
        let now = self.amount.clone() * earlier.shares.clone();
        let before = earlier.amount.clone() * self.shares.clone();
        let difference = if now > before { now - before.clone() } else { before.clone() - now };
        difference * BigInt::from(1_000_000_000_000u64) > before
    }
}

/// One `TransferShares` log, decoded.
pub struct SharesMove {
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub shares: BigInt,
}

pub fn decode_transfer_shares(log: &eth::Log) -> Option<SharesMove> {
    if log.topics.len() != 3 || log.topics[0] != TRANSFER_SHARES || log.data.len() != 32 {
        return None;
    }
    Some(SharesMove {
        from: log.topics[1][12..].to_vec(),
        to: log.topics[2][12..].to_vec(),
        shares: BigInt::from_unsigned_bytes_be(&log.data),
    })
}

/// The ratio each `TransferShares` in `logs` reveals: its `shares` against the amount of the
/// `Transfer` the same token emitted just before it, between the same two addresses, with the
/// ordinal of the `TransferShares`.
///
/// Lido and its forks emit the pair back to back for every transfer, mint and burn, and so do
/// xStocks tokens. A pair with nothing on either side tells nothing and is skipped.
pub fn ratio_pairs<'a>(logs: &[&'a eth::Log]) -> Vec<(&'a [u8], ShareRatio, u64)> {
    let mut pairs = Vec::new();
    for window in logs.windows(2) {
        let (transfer, shares) = (window[0], window[1]);
        let Some(moved) = decode_transfer_shares(shares) else { continue };
        let is_matching_transfer = transfer.address == shares.address &&
            transfer.topics.len() == 3 &&
            transfer.topics[0] == TRANSFER &&
            transfer.topics[1] == shares.topics[1] &&
            transfer.topics[2] == shares.topics[2] &&
            transfer.data.len() == 32;
        if !is_matching_transfer {
            continue;
        }
        let amount = BigInt::from_unsigned_bytes_be(&transfer.data);
        if amount == BigInt::zero() || moved.shares == BigInt::zero() {
            continue;
        }
        pairs.push((
            shares.address.as_slice(),
            ShareRatio { amount, shares: moved.shares },
            shares.ordinal,
        ));
    }
    pairs
}

/// The pair with the most shares, per token, among a block's pairs. The larger the pair, the less
/// its rounding shows in the ratio.
pub fn best_ratios(pairs: Vec<(&[u8], ShareRatio, u64)>) -> Vec<(&[u8], ShareRatio, u64)> {
    let mut best: Vec<(&[u8], ShareRatio, u64)> = Vec::new();
    for (token, ratio, ordinal) in pairs {
        match best
            .iter_mut()
            .find(|(seen, _, _)| *seen == token)
        {
            Some((_, kept, _)) if kept.shares >= ratio.shares => {}
            Some(kept) => *kept = (token, ratio, ordinal),
            None => best.push((token, ratio, ordinal)),
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRCX: [u8; 20] = hex!("1aad217b8f78dba5e6693460e8470f8b1a3977f3");
    const ALICE: [u8; 20] = [0xa1; 20];
    const BOB: [u8; 20] = [0xb0; 20];

    fn topic(address: &[u8]) -> Vec<u8> {
        let mut topic = vec![0u8; 12];
        topic.extend_from_slice(address);
        topic
    }

    fn log(token: [u8; 20], topic0: [u8; 32], from: &[u8], to: &[u8], value: u128) -> eth::Log {
        let mut data = vec![0u8; 16];
        data.extend_from_slice(&value.to_be_bytes());
        eth::Log {
            address: token.to_vec(),
            topics: vec![topic0.to_vec(), topic(from), topic(to)],
            data,
            ..Default::default()
        }
    }

    /// A pair from an STRCx transfer on mainnet (block 26,228,7xx): 101,164,227,420,775,278 units
    /// for 92,194,940,671,436,393 shares, a multiplier of about 1.0973.
    #[test]
    fn a_transfer_and_its_shares_give_the_ratio() {
        let transfer = log(STRCX, TRANSFER, &ALICE, &BOB, 101_164_227_420_775_278);
        let shares = log(STRCX, TRANSFER_SHARES, &ALICE, &BOB, 92_194_940_671_436_393);
        let pairs = ratio_pairs(&[&transfer, &shares]);
        assert_eq!(pairs.len(), 1);
        let ratio = &pairs[0].1;
        assert_eq!(ratio.amount, BigInt::from(101_164_227_420_775_278u64));
        assert_eq!(ratio.shares, BigInt::from(92_194_940_671_436_393u64));
        // Converting the pair's own shares back gives its amount, give or take the rounding.
        assert_eq!(ratio.to_amount(&ratio.shares), ratio.amount);
    }

    /// A `TransferShares` that does not follow its own `Transfer` names no ratio: another token, or
    /// another pair of addresses, does not count.
    #[test]
    fn only_a_matching_transfer_pairs() {
        let other_token = log([0x77; 20], TRANSFER, &ALICE, &BOB, 100);
        let other_holders = log(STRCX, TRANSFER, &BOB, &ALICE, 100);
        let shares = log(STRCX, TRANSFER_SHARES, &ALICE, &BOB, 90);
        assert!(ratio_pairs(&[&other_token, &shares]).is_empty());
        assert!(ratio_pairs(&[&other_holders, &shares]).is_empty());
    }

    #[test]
    fn the_largest_pair_sets_the_ratio() {
        let small = ShareRatio { amount: BigInt::from(3), shares: BigInt::from(2) };
        let large = ShareRatio { amount: BigInt::from(1_097_300), shares: BigInt::from(1_000_000) };
        let best = best_ratios(vec![(&STRCX[..], small, 1), (&STRCX[..], large.clone(), 2)]);
        assert_eq!(best, vec![(&STRCX[..], large, 2)]);
    }

    /// Rounding between two transfers at one ratio is not a move; a rebase-sized step is.
    #[test]
    fn only_a_real_move_of_the_ratio_counts() {
        let before = ShareRatio {
            amount: BigInt::from(1_220_000_000_000_000_000u64),
            shares: BigInt::from(1_000_000_000_000_000_000u64),
        };
        let rounding = ShareRatio {
            amount: BigInt::from(1_220_000_000_000_000_001u64),
            shares: BigInt::from(1_000_000_000_000_000_000u64),
        };
        let rebase = ShareRatio {
            amount: BigInt::from(1_220_100_000_000_000_000u64),
            shares: BigInt::from(1_000_000_000_000_000_000u64),
        };
        assert!(!rounding.moved_from(&before));
        assert!(rebase.moved_from(&before));
        assert!(before.moved_from(&rebase));
    }

    #[test]
    fn a_ratio_round_trips_through_the_store() {
        let ratio = ShareRatio { amount: BigInt::from(1_097_300), shares: BigInt::from(1_000_000) };
        assert_eq!(ShareRatio::decode(&ratio.encode()), Some(ratio));
        assert_eq!(ShareRatio::decode("0:5"), None);
        assert_eq!(ShareRatio::decode("garbage"), None);
    }
}
