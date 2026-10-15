//! SY (ERC-5115) profiling and per-token conversion classification.
//!
//! Every SY wraps a different protocol, so there is no single deposit/redeem formula. What is
//! generic is the *evidence*: `previewDeposit` / `previewRedeem` are pure views, so one probe of
//! a single token unit at creation says which of two closed-form conversions — if either — the
//! SY implements for that token. Tokens matching neither are left out of the component, which
//! is what makes an unquotable SY edge absent rather than silently wrong.
//!
//! Classification is per `(SY, token)` pair, not per SY: wrappers commonly accept their base
//! token 1:1 *and* the accounting asset at the index rate.

use substreams::{log, scalar::BigInt};
use substreams_ethereum::{pb::eth::rpc::RpcResponse, rpc::RpcBatch};
use tycho_substreams::{abi::erc20, prelude::*};

use crate::{abi::pendle_sy, consts::PENDLE_SY, keys::contract_id};

/// Which closed-form conversion an SY applies to one specific token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenClass {
    /// One token unit is one SY unit, up to a decimal rescale.
    OneToOne,
    /// The token is treated as the accounting asset and converted at `exchangeRate()`.
    IndexRate,
}

impl TokenClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenClass::OneToOne => "one_to_one",
            TokenClass::IndexRate => "index_rate",
        }
    }
}

/// Fixed-point scale of `SY.exchangeRate()`. The rate is always 1e18-scaled even where the SY
/// and its accounting asset have different decimals — the decimal gap is absorbed into the rate
/// itself, so it must never be treated as `10^sy_decimals`.
const RATE_ONE: u64 = 1_000_000_000_000_000_000;

/// Relative tolerance for matching a probe against a prediction, as a reciprocal: a prediction is
/// accepted when it is within 1/1e6 of the observed value.
///
/// The bound is set by the spread the SYs charge on entry, not by rounding. Swept over the 46 live
/// SYs at one block, every `previewRedeem` probe matches its prediction to the unit, and the
/// `previewDeposit` probes that do not split in two: one or two wei where the SY converts through
/// the wrapped protocol's own floor-divide first, and seven SYs taking a real 1e-7 to 1e-6 haircut
/// on entry. Those seven are what a wei-level tolerance would drop. It stays far under a haircut
/// large enough to mean the SY is doing something else entirely (0.5% on `0x457904b5...59d6`), so
/// a token matching neither closed form is still excluded rather than rounded into the nearer
/// class, and the two predictions never both match unless they are arithmetically identical.
const TOLERANCE_RECIPROCAL: u64 = 1_000_000;

pub fn pow10(exponent: u32) -> BigInt {
    BigInt::from(10).pow(exponent)
}

/// True when `observed` is within the relative tolerance of `predicted`.
fn matches(observed: &BigInt, predicted: &BigInt) -> bool {
    if predicted.clone() == BigInt::zero() {
        return false;
    }
    let difference = if observed > predicted {
        observed.clone() - predicted.clone()
    } else {
        predicted.clone() - observed.clone()
    };
    difference * BigInt::from(TOLERANCE_RECIPROCAL) <= predicted.clone()
}

/// Picks the class whose prediction matches the probe.
///
/// `OneToOne` is only asserted where the two predictions genuinely diverge. Where they coincide
/// — an SY whose `exchangeRate` currently sits at exactly one unit — the evidence cannot
/// separate them, and the tie goes to `IndexRate`: it *is* the identity while the rate is one,
/// and it stays correct once the rate moves away. Freezing such an SY as `OneToOne` would leave
/// the label right today and quietly wrong by `rate - 1` forever after.
fn classify(observed: &BigInt, one_to_one: &BigInt, index_rate: &BigInt) -> Option<TokenClass> {
    if matches(observed, one_to_one) && !matches(one_to_one, index_rate) {
        return Some(TokenClass::OneToOne);
    }
    if matches(observed, index_rate) {
        return Some(TokenClass::IndexRate);
    }
    None
}

/// Classifies a `tokensIn` entry from a `previewDeposit` probe of one whole token unit.
///
/// `observed` must be the SY-unit output the SY predicts for exactly `10^token_decimals` of the
/// token. Both predictions are then independent of the token's own decimals, which cancel
/// against the probe amount.
pub fn classify_deposit(
    observed: &BigInt,
    sy_decimals: u32,
    asset_decimals: u32,
    exchange_rate: &BigInt,
) -> Option<TokenClass> {
    if exchange_rate.clone() == BigInt::zero() {
        return None;
    }
    let one_to_one = pow10(sy_decimals);
    // The token enters as the accounting asset, then converts to SY: `assetToSy` divides by the
    // rate, and the rate carries the sy/asset decimal gap, so the quotient is already in SY
    // units.
    let index_rate = pow10(asset_decimals) * BigInt::from(RATE_ONE) / exchange_rate.clone();
    classify(observed, &one_to_one, &index_rate)
}

/// Classifies a `tokensOut` entry from a `previewRedeem` probe of one whole SY unit.
///
/// `observed` is the token output the SY predicts for `10^sy_decimals` of itself.
pub fn classify_redeem(
    observed: &BigInt,
    token_decimals: u32,
    sy_decimals: u32,
    asset_decimals: u32,
    exchange_rate: &BigInt,
) -> Option<TokenClass> {
    if exchange_rate.clone() == BigInt::zero() {
        return None;
    }
    let one_to_one = pow10(token_decimals);
    // Mirror of the deposit direction: `syToAsset` multiplies by the rate, leaving asset-decimal
    // units, which are then rescaled to the token's own decimals.
    let asset_amount = pow10(sy_decimals) * exchange_rate.clone() / BigInt::from(RATE_ONE);
    let index_rate = asset_amount * pow10(token_decimals) / pow10(asset_decimals);
    classify(observed, &one_to_one, &index_rate)
}

/// An SY resolved far enough to quote its wrap edges.
pub(crate) struct SyProfile {
    pub(crate) address: Vec<u8>,
    pub(crate) decimals: u32,
    pub(crate) asset: Vec<u8>,
    pub(crate) asset_decimals: u32,
    /// Quotable entry tokens and the conversion each uses.
    pub(crate) tokens_in: Vec<(Vec<u8>, TokenClass)>,
    /// Quotable exit tokens and the conversion each uses.
    pub(crate) tokens_out: Vec<(Vec<u8>, TokenClass)>,
    /// `totalSupply()` at the creation block, the seed supply is accumulated from.
    pub(crate) total_supply: BigInt,
    /// The SY's `balanceOf` in each of its component tokens at the creation block, the seeds its
    /// balances are accumulated from. A token whose read failed is absent and starts at zero.
    pub(crate) holdings: Vec<(Vec<u8>, BigInt)>,
    /// `paused()` at the creation block; `Paused` / `Unpaused` move it afterwards.
    pub(crate) paused: bool,
}

/// State attribute carrying an SY's share supply, which bounds every redemption from it.
pub const SY_TOTAL_SUPPLY: &str = "sy_total_supply";

/// Static attribute carrying an SY's `totalSupply()` at creation. Consumed by
/// `map_reserve_deltas` and stripped before the component is emitted.
pub const CREATION_TOTAL_SUPPLY: &str = "creation_total_supply";
/// Prefix of the static attributes carrying an SY's holding of each token at creation, followed by
/// the token's hex address. Consumed by `map_relative_component_balance` and stripped before the
/// component is emitted.
pub const CREATION_BALANCE_PREFIX: &str = "creation_balance_0x";

/// Static attribute carrying whether an SY was paused at creation. Consumed by `map_sy_state` and
/// stripped before the component is emitted.
pub const CREATION_PAUSED: &str = "creation_paused";
/// State attribute: whether the SY is paused, which refuses every quote against it.
pub const PAUSED: &str = "paused";
/// Prefixes of the per-token class attributes, followed by the token's hex address. Emitted as
/// state, since an upgrade can change how a token converts.
pub const TOKEN_IN_CLASS_PREFIX: &str = "token_in_class_0x";
pub const TOKEN_OUT_CLASS_PREFIX: &str = "token_out_class_0x";

/// Whether a static attribute is a creation seed, which only seeds the stores and is never emitted.
pub fn is_creation_seed(name: &str) -> bool {
    name == CREATION_TOTAL_SUPPLY ||
        name == CREATION_PAUSED ||
        name.starts_with(CREATION_BALANCE_PREFIX)
}

/// Whether an attribute is a token class.
pub fn is_class_attribute(name: &str) -> bool {
    name.starts_with(TOKEN_IN_CLASS_PREFIX) || name.starts_with(TOKEN_OUT_CLASS_PREFIX)
}

/// One class attribute per quotable token and direction: an entry token and an exit token can
/// convert differently on the same SY, so the class is recorded per direction.
pub fn class_attributes(
    tokens_in: &[(Vec<u8>, TokenClass)],
    tokens_out: &[(Vec<u8>, TokenClass)],
) -> Vec<(String, Vec<u8>)> {
    let mut attributes = Vec::new();
    for (token, class) in tokens_in {
        attributes.push((
            format!("{TOKEN_IN_CLASS_PREFIX}{}", hex::encode(token)),
            class.as_str().as_bytes().to_vec(),
        ));
    }
    for (token, class) in tokens_out {
        attributes.push((
            format!("{TOKEN_OUT_CLASS_PREFIX}{}", hex::encode(token)),
            class.as_str().as_bytes().to_vec(),
        ));
    }
    attributes
}

/// The SY's supply at creation, from its seed attribute.
pub fn supply_seed(component: &ProtocolComponent) -> Option<BigInt> {
    component
        .get_attribute_value(CREATION_TOTAL_SUPPLY)
        .map(|raw| BigInt::from_signed_bytes_be(&raw))
}

/// The SY's holding of each token at creation, from its seed attributes.
pub fn balance_seeds(component: &ProtocolComponent) -> Vec<(Vec<u8>, BigInt)> {
    let mut seeds = Vec::new();
    for attribute in &component.static_att {
        let Some(token) = attribute
            .name
            .strip_prefix(CREATION_BALANCE_PREFIX)
        else {
            continue;
        };
        let token = hex::decode(token).expect("creation balance attribute names a non-hex token");
        seeds.push((token, BigInt::from_signed_bytes_be(&attribute.value)));
    }
    seeds
}

impl SyProfile {
    /// The component's tokens: the SY itself, then every quotable token once.
    fn component_tokens(&self) -> Vec<Vec<u8>> {
        component_tokens(&self.address, &self.tokens_in, &self.tokens_out)
    }

    pub(crate) fn into_component(self) -> ProtocolComponent {
        let tokens = self.component_tokens();

        // The classes ride here only as far as `map_protocol_changes`, which moves them to state
        // so an upgrade can change them.
        let mut attributes = vec![
            ("asset_address".to_string(), self.asset.clone()),
            ("asset_decimals".to_string(), BigInt::from(self.asset_decimals).to_signed_bytes_be()),
            ("sy_decimals".to_string(), BigInt::from(self.decimals).to_signed_bytes_be()),
        ];
        attributes.extend(class_attributes(&self.tokens_in, &self.tokens_out));
        attributes.push((CREATION_PAUSED.to_string(), vec![u8::from(self.paused)]));
        attributes
            .push((CREATION_TOTAL_SUPPLY.to_string(), self.total_supply.to_signed_bytes_be()));
        for (token, balance) in &self.holdings {
            attributes.push((
                format!("{CREATION_BALANCE_PREFIX}{}", hex::encode(token)),
                balance.to_signed_bytes_be(),
            ));
        }

        ProtocolComponent::new(&contract_id(&self.address))
            .with_tokens(&tokens)
            .with_attributes(&attributes)
            .as_swap_type(PENDLE_SY, ImplementationType::Custom)
    }
}

pub(crate) fn profile_sy(sy_address: &[u8]) -> Option<SyProfile> {
    let Classification { decimals, asset, asset_decimals, tokens_in, tokens_out } =
        classify_sy(sy_address)?;

    let tokens = component_tokens(sy_address, &tokens_in, &tokens_out);
    let Some(Seeds { total_supply, holdings, paused }) = read_seeds(sy_address, &tokens) else {
        log::info!("SY 0x{} did not answer totalSupply()", hex::encode(sy_address));
        return None;
    };

    Some(SyProfile {
        address: sy_address.to_vec(),
        decimals,
        asset,
        asset_decimals,
        tokens_in,
        tokens_out,
        total_supply,
        holdings,
        paused,
    })
}

/// What a classification of an SY's tokens found: its units, and each quotable token's class.
pub(crate) struct Classification {
    pub(crate) decimals: u32,
    pub(crate) asset: Vec<u8>,
    pub(crate) asset_decimals: u32,
    /// Quotable entry tokens and the conversion each uses.
    pub(crate) tokens_in: Vec<(Vec<u8>, TokenClass)>,
    /// Quotable exit tokens and the conversion each uses.
    pub(crate) tokens_out: Vec<(Vec<u8>, TokenClass)>,
}

/// Classifies each token the SY lists by probing `previewDeposit` / `previewRedeem` against the
/// two closed forms, at the current block. `None` when the SY's units do not resolve.
pub(crate) fn classify_sy(sy_address: &[u8]) -> Option<Classification> {
    let responses = RpcBatch::new()
        .add(pendle_sy::functions::Decimals {}, sy_address.to_vec())
        .add(pendle_sy::functions::AssetInfo {}, sy_address.to_vec())
        .add(pendle_sy::functions::ExchangeRate {}, sy_address.to_vec())
        .add(pendle_sy::functions::GetTokensIn {}, sy_address.to_vec())
        .add(pendle_sy::functions::GetTokensOut {}, sy_address.to_vec())
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let decimals = decode_decimals(responses.first()?)?;
    let (_asset_type, asset, asset_decimals) = responses
        .get(1)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::AssetInfo>)?;
    let exchange_rate = responses
        .get(2)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::ExchangeRate>)?;
    // The token lists are optional: an SY that will not enumerate them still yields a quotable
    // market, it just contributes no wrap edges.
    let tokens_in = responses
        .get(3)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::GetTokensIn>)
        .unwrap_or_default();
    let tokens_out = responses
        .get(4)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::GetTokensOut>)
        .unwrap_or_default();

    let asset_decimals = asset_decimals.to_u64() as u32;
    let token_decimals = read_token_decimals(&tokens_in, &tokens_out);

    let mut quotable_in = Vec::new();
    for (token, probe) in probe_deposits(sy_address, &tokens_in, &token_decimals) {
        let Some(class) = classify_deposit(&probe, decimals, asset_decimals, &exchange_rate) else {
            continue;
        };
        quotable_in.push((token, class));
    }

    let mut quotable_out = Vec::new();
    for (token, token_decimals, probe) in
        probe_redeems(sy_address, &tokens_out, &token_decimals, decimals)
    {
        let Some(class) =
            classify_redeem(&probe, token_decimals, decimals, asset_decimals, &exchange_rate)
        else {
            continue;
        };
        quotable_out.push((token, class));
    }

    Some(Classification {
        decimals,
        asset,
        asset_decimals,
        tokens_in: quotable_in,
        tokens_out: quotable_out,
    })
}

fn component_tokens(
    sy_address: &[u8],
    tokens_in: &[(Vec<u8>, TokenClass)],
    tokens_out: &[(Vec<u8>, TokenClass)],
) -> Vec<Vec<u8>> {
    let mut tokens = vec![sy_address.to_vec()];
    for (token, _) in tokens_in
        .iter()
        .chain(tokens_out.iter())
    {
        if !tokens.contains(token) {
            tokens.push(token.clone());
        }
    }
    tokens
}

/// Reads the SY's supply and its holding of each token in one batch, at the creation block.
///
/// These are the only reads its supply and balances ever need: afterwards both are accumulated
/// from events. `None` when the supply does not resolve, since a component without one could not
/// bound a redemption.
struct Seeds {
    total_supply: BigInt,
    holdings: Vec<(Vec<u8>, BigInt)>,
    paused: bool,
}

fn read_seeds(sy_address: &[u8], tokens: &[Vec<u8>]) -> Option<Seeds> {
    let mut batch = RpcBatch::new()
        .add(erc20::functions::TotalSupply {}, sy_address.to_vec())
        .add(pendle_sy::functions::Paused {}, sy_address.to_vec());
    for token in tokens {
        batch =
            batch.add(erc20::functions::BalanceOf { owner: sy_address.to_vec() }, token.clone());
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let total_supply = RpcBatch::decode::<_, erc20::functions::TotalSupply>(responses.first()?)?;
    // Five SYs have no `paused()` at all; they cannot be paused, which is what `false` says.
    let paused = responses
        .get(1)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::Paused>)
        .unwrap_or(false);
    let mut holdings = Vec::new();
    for (token, response) in tokens
        .iter()
        .zip(responses.iter().skip(2))
    {
        let Some(balance) = RpcBatch::decode::<_, erc20::functions::BalanceOf>(response) else {
            log::info!(
                "SY 0x{} did not answer balanceOf for 0x{}, its balance starts at zero",
                hex::encode(sy_address),
                hex::encode(token)
            );
            continue;
        };
        holdings.push((token.clone(), balance));
    }
    Some(Seeds { total_supply, holdings, paused })
}

fn decode_decimals(response: &RpcResponse) -> Option<u32> {
    RpcBatch::decode::<_, pendle_sy::functions::Decimals>(response).map(|d| d.to_u64() as u32)
}

/// Reads `decimals()` for every token either side of the SY in one batch.
///
/// Native ETH is listed by some SYs as the zero address and has no `decimals()`; it drops out
/// here and is therefore never classified or declared.
fn read_token_decimals(tokens_in: &[Vec<u8>], tokens_out: &[Vec<u8>]) -> Vec<(Vec<u8>, u32)> {
    let mut unique: Vec<Vec<u8>> = Vec::new();
    for token in tokens_in
        .iter()
        .chain(tokens_out.iter())
    {
        if token.iter().all(|b| *b == 0) || unique.contains(token) {
            continue;
        }
        unique.push(token.clone());
    }

    let mut batch = RpcBatch::new();
    for token in &unique {
        batch = batch.add(erc20::functions::Decimals {}, token.clone());
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let mut decimals = Vec::new();
    for (token, response) in unique.into_iter().zip(responses.iter()) {
        let Some(value) = RpcBatch::decode::<_, erc20::functions::Decimals>(response) else {
            continue;
        };
        decimals.push((token, value.to_u64() as u32));
    }
    decimals
}

/// Probes `previewDeposit` with one whole unit of each entry token.
fn probe_deposits(
    sy_address: &[u8],
    tokens_in: &[Vec<u8>],
    token_decimals: &[(Vec<u8>, u32)],
) -> Vec<(Vec<u8>, BigInt)> {
    let mut probes = Vec::new();
    for token in tokens_in {
        let Some((_, decimals)) = token_decimals
            .iter()
            .find(|(t, _)| t == token)
        else {
            continue;
        };
        probes.push((token.clone(), *decimals));
    }

    let mut batch = RpcBatch::new();
    for (token, decimals) in &probes {
        batch = batch.add(
            pendle_sy::functions::PreviewDeposit {
                token_in: token.clone(),
                amount_token_to_deposit: pow10(*decimals),
            },
            sy_address.to_vec(),
        );
    }

    let mut results = Vec::new();
    for ((token, _), value) in probes
        .into_iter()
        .zip(execute_probes::<pendle_sy::functions::PreviewDeposit>(batch))
    {
        let Some(value) = value else { continue };
        results.push((token, value));
    }
    results
}

/// Probes `previewRedeem` with one whole SY unit for each exit token.
///
/// Returns each token with its own decimals, which the redeem conversion needs to rescale the
/// asset-unit result the SY reports.
fn probe_redeems(
    sy_address: &[u8],
    tokens_out: &[Vec<u8>],
    token_decimals: &[(Vec<u8>, u32)],
    sy_decimals: u32,
) -> Vec<(Vec<u8>, u32, BigInt)> {
    let mut probes = Vec::new();
    for token in tokens_out {
        let Some((_, decimals)) = token_decimals
            .iter()
            .find(|(t, _)| t == token)
        else {
            continue;
        };
        probes.push((token.clone(), *decimals));
    }

    let mut batch = RpcBatch::new();
    for (token, _) in &probes {
        batch = batch.add(
            pendle_sy::functions::PreviewRedeem {
                token_out: token.clone(),
                amount_shares_to_redeem: pow10(sy_decimals),
            },
            sy_address.to_vec(),
        );
    }

    let mut results = Vec::new();
    for ((token, decimals), value) in probes
        .into_iter()
        .zip(execute_probes::<pendle_sy::functions::PreviewRedeem>(batch))
    {
        let Some(value) = value else { continue };
        results.push((token, decimals, value));
    }
    results
}

/// Executes a probe batch, yielding one entry per call in the order they were added.
///
/// A reverting probe — a paused SY, a token the view rejects — yields `None`, which leaves that
/// token out of the component rather than guessed at.
fn execute_probes<F>(batch: RpcBatch) -> Vec<Option<BigInt>>
where
    F: substreams_ethereum::Function + substreams_ethereum::rpc::RPCDecodable<BigInt>,
{
    batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default()
        .iter()
        .map(RpcBatch::decode::<_, F>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixture below is a value read from Ethereum mainnet, not one computed from this
    /// module's own constants. That matters: predictions derived from `RATE_ONE` would agree
    /// with a wrongly-scaled `RATE_ONE` and the tests would pass while every quote was wrong.
    ///
    /// Every fixture within one module is a single-block snapshot: the rate and the previews
    /// must come from the same block, or the rate accrues between reads and the prediction is
    /// compared against a probe taken under a different rate.
    ///
    /// SY-wstETH `0xcbc72d92...c0bc` at block 25803485 -- 18-decimal SY over 18-decimal stETH.
    mod wsteth_sy {
        pub const SY_DECIMALS: u32 = 18;
        pub const ASSET_DECIMALS: u32 = 18;
        pub const EXCHANGE_RATE: u64 = 1_242_190_142_783_145_750;
        /// `previewDeposit(wstETH, 1e18)` -- the SY's own wrapper token, taken at par.
        pub const DEPOSIT_WSTETH: u64 = 1_000_000_000_000_000_000;
        /// `previewDeposit(stETH, 1e18)` -- the accounting asset, converted at the rate.
        pub const DEPOSIT_STETH: u64 = 805_029_733_821_172_431;
        /// `previewRedeem(1e18)` to stETH.
        pub const REDEEM_STETH: u64 = 1_242_190_142_783_145_750;
        /// `previewRedeem(1e18)` to wstETH.
        pub const REDEEM_WSTETH: u64 = 1_000_000_000_000_000_000;
    }

    /// SY `0x457904b5...59d6` at block 25813586 -- 18-decimal SY over 6-decimal USDC. The
    /// decimal gap lives in the rate, which is why the rate is ~1.12e6 rather than ~1.12e18.
    mod usdc_sy {
        pub const SY_DECIMALS: u32 = 18;
        pub const ASSET_DECIMALS: u32 = 6;
        pub const USDC_DECIMALS: u32 = 6;
        pub const EXCHANGE_RATE: u64 = 1_121_519;
        /// `previewDeposit(USDC, 1e6)`.
        pub const DEPOSIT_USDC: u64 = 891_647_606_956_028_309;
        /// `previewRedeem(1e18)` to USDC. Below the rate: this SY takes a haircut on exit, so
        /// neither closed form explains it.
        pub const REDEEM_USDC: u64 = 1_115_911;
    }

    /// SY-siUSD `0x9f30507c...a920` at block 25810474 -- 18-decimal SY over an 18-decimal asset
    /// that also redeems to 6-decimal USDC. The only live shape where the exit token's decimals
    /// differ from the accounting asset's, so it is the only fixture that exercises the redeem
    /// rescale.
    mod cross_decimal_sy {
        pub const SY_DECIMALS: u32 = 18;
        pub const ASSET_DECIMALS: u32 = 18;
        pub const USDC_DECIMALS: u32 = 6;
        pub const EXCHANGE_RATE: u64 = 1_084_785_195_710_495_612;
        /// `previewRedeem(1e18)` to USDC.
        pub const REDEEM_USDC: u64 = 1_084_785;
    }

    /// A real wrapper taken at par, where the two predictions are far apart -- the only case in
    /// which `OneToOne` is provable.
    #[test]
    fn wsteth_deposit_of_wrapper_token_is_one_to_one() {
        assert_eq!(
            classify_deposit(
                &BigInt::from(wsteth_sy::DEPOSIT_WSTETH),
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::OneToOne)
        );
    }

    /// The same SY converts its accounting asset at the rate. The observed value sits one wei
    /// above the exact quotient, so this also pins the tolerance.
    #[test]
    fn wsteth_deposit_of_accounting_asset_is_index_rate() {
        assert_eq!(
            classify_deposit(
                &BigInt::from(wsteth_sy::DEPOSIT_STETH),
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
    }

    #[test]
    fn wsteth_redeem_splits_by_token() {
        assert_eq!(
            classify_redeem(
                &BigInt::from(wsteth_sy::REDEEM_STETH),
                wsteth_sy::ASSET_DECIMALS,
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
        assert_eq!(
            classify_redeem(
                &BigInt::from(wsteth_sy::REDEEM_WSTETH),
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::OneToOne)
        );
    }

    /// The decimal axis: an 18-decimal SY over a 6-decimal asset. Fails if the index is treated
    /// as `10^sy_decimals` instead of 1e18.
    #[test]
    fn deposit_across_a_decimal_gap_is_index_rate() {
        assert_eq!(
            classify_deposit(
                &BigInt::from(usdc_sy::DEPOSIT_USDC),
                usdc_sy::SY_DECIMALS,
                usdc_sy::ASSET_DECIMALS,
                &BigInt::from(usdc_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
    }

    /// The tolerance is the widest entry haircut a quotable SY may take, because the simulation
    /// quotes the closed form and so over-quotes such a deposit by the haircut. The widest the
    /// sweep found is 1e-6; this pins that a haircut at 1e-6 still classifies and anything wider,
    /// 5e-6 included, is left out rather than quoted five times worse.
    #[test]
    fn an_entry_haircut_classifies_up_to_one_in_a_million() {
        let one_to_one = BigInt::from(1_000_000_000_000_000_000u64);
        let classify_at = |observed: BigInt| {
            classify_deposit(
                &observed,
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            )
        };
        let one_in_a_million = BigInt::from(1_000_000_000_000u64);

        assert_eq!(
            classify_at(one_to_one.clone() - one_in_a_million.clone()),
            Some(TokenClass::OneToOne)
        );
        assert_eq!(
            classify_at(one_to_one.clone() - one_in_a_million.clone() - BigInt::from(1)),
            None
        );
        assert_eq!(classify_at(one_to_one - one_in_a_million * BigInt::from(5)), None);
    }

    /// A real exit that neither closed form explains -- this SY redeems ~0.5% below the rate.
    /// It must be excluded, not rounded into the nearer class.
    #[test]
    fn redeem_with_a_haircut_is_rejected() {
        assert_eq!(
            classify_redeem(
                &BigInt::from(usdc_sy::REDEEM_USDC),
                usdc_sy::USDC_DECIMALS,
                usdc_sy::SY_DECIMALS,
                usdc_sy::ASSET_DECIMALS,
                &BigInt::from(usdc_sy::EXCHANGE_RATE),
            ),
            None
        );
    }

    /// Redeeming an 18-decimal SY into a 6-decimal token: the asset-unit result has to be
    /// rescaled to the exit token's decimals. Without that rescale the prediction is off by
    /// twelve orders of magnitude and the token would be dropped as unquotable.
    #[test]
    fn redeem_rescales_to_the_exit_token_decimals() {
        assert_eq!(
            classify_redeem(
                &BigInt::from(cross_decimal_sy::REDEEM_USDC),
                cross_decimal_sy::USDC_DECIMALS,
                cross_decimal_sy::SY_DECIMALS,
                cross_decimal_sy::ASSET_DECIMALS,
                &BigInt::from(cross_decimal_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
    }

    /// A rate of exactly one unit makes both predictions identical, so creation-time evidence
    /// cannot separate them and the tie must go to `IndexRate`.
    #[test]
    fn rate_of_one_ties_to_index_rate() {
        let observed = pow10(18);
        let rate = BigInt::from(RATE_ONE);
        assert_eq!(classify_deposit(&observed, 18, 18, &rate), Some(TokenClass::IndexRate));
        assert_eq!(classify_redeem(&observed, 18, 18, 18, &rate), Some(TokenClass::IndexRate));
    }

    /// A zero rate is a paused or uninitialised SY, not a 1:1 wrapper.
    #[test]
    fn zero_rate_is_rejected() {
        let observed = pow10(18);
        assert_eq!(classify_deposit(&observed, 18, 18, &BigInt::zero()), None);
        assert_eq!(classify_redeem(&observed, 18, 18, 18, &BigInt::zero()), None);
    }
}
