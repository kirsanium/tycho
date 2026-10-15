//! Decoding a Tycho snapshot into `PendleState`.
//!
//! Tycho binds one state type per protocol system, so this dispatches on `protocol_type_name` —
//! the two component types the Substreams package emits carry entirely different attributes.
//!
//! Every attribute read here is one the indexing PR emits. A missing one is an error rather than a
//! default, with two exceptions that both describe a real on-chain state rather than a guess:
//!
//! - A market's reserves and implied rate are absent until its first liquidity event, because the
//!   indexer only writes them from events. A market's storage starts zeroed, so absent means zero.
//! - A rate (`py_index_current`, `sy_exchange_rate`) is absent until a refresh first reads it. The
//!   state then decodes as never sampled, which no head counts as current: it refuses every quote
//!   and reports no depth until a delta brings the rate in.
//!
//! Failing either case instead would lose the component for the life of the stream, since a
//! component that fails to decode is never decoded again.

use std::{collections::HashMap, str::FromStr};

use alloy::primitives::{I256, U256};
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{
    clock::NEVER_SAMPLED,
    math::market::MarketState,
    state::{
        attribute_i256, attribute_u256, PendleMarketState, PendleState, PendleSyState, TokenClass,
        ValueTooWide, PAUSED, SY_TOTAL_SUPPLY, TOKEN_IN_CLASS_PREFIX, TOKEN_OUT_CLASS_PREFIX,
    },
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

const MARKET_TYPE: &str = "pendle_market";
const SY_TYPE: &str = "pendle_sy";

const ATTR_SCALAR_ROOT: &str = "scalar_root";
const ATTR_EXPIRY: &str = "expiry";
const ATTR_SY_ADDRESS: &str = "sy_address";
const ATTR_PT_ADDRESS: &str = "pt_address";
const ATTR_YT_ADDRESS: &str = "yt_address";
const ATTR_TOTAL_PT: &str = "total_pt";
const ATTR_TOTAL_SY: &str = "total_sy";
const ATTR_LAST_LN_IMPLIED_RATE: &str = "last_ln_implied_rate";
const ATTR_LN_FEE_RATE_ROOT: &str = "ln_fee_rate_root";
const ATTR_RESERVE_FEE_PERCENT: &str = "reserve_fee_percent";
const ATTR_PY_INDEX_CURRENT: &str = "py_index_current";
const ATTR_RATE_SAMPLED_AT: &str = "rate_sampled_at";
const ATTR_SY_DECIMALS: &str = "sy_decimals";
const ATTR_ASSET_DECIMALS: &str = "asset_decimals";
const ATTR_SY_EXCHANGE_RATE: &str = "sy_exchange_rate";

impl From<ValueTooWide> for InvalidSnapshotError {
    fn from(error: ValueTooWide) -> Self {
        InvalidSnapshotError::ValueError(error.to_string())
    }
}

fn missing(name: &str) -> InvalidSnapshotError {
    InvalidSnapshotError::MissingAttribute(name.to_string())
}

fn address(raw: &Bytes, name: &str) -> Result<Bytes, InvalidSnapshotError> {
    if raw.len() != 20 {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{name} is {} bytes, expected a 20-byte address",
            raw.len()
        )));
    }
    Ok(raw.clone())
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for PendleState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        match snapshot
            .component
            .protocol_type_name
            .as_str()
        {
            MARKET_TYPE => decode_market(snapshot, block).map(PendleState::Market),
            SY_TYPE => decode_sy(snapshot, block, all_tokens).map(PendleState::Sy),
            other => Err(InvalidSnapshotError::ValueError(format!(
                "unknown Pendle protocol type {other}"
            ))),
        }
    }
}

fn decode_market(
    snapshot: ComponentWithState,
    block: BlockHeader,
) -> Result<PendleMarketState, InvalidSnapshotError> {
    let statics = &snapshot.component.static_attributes;
    let state = &snapshot.state.attributes;

    let scalar_root =
        attribute_i256(statics, ATTR_SCALAR_ROOT).ok_or_else(|| missing(ATTR_SCALAR_ROOT))?;
    let expiry = attribute_u256(statics, ATTR_EXPIRY)?
        .ok_or_else(|| missing(ATTR_EXPIRY))?
        .saturating_to::<u64>();

    // Written only from Mint, Burn, Swap and UpdateImpliedRate, so a market that has not seen its
    // first liquidity yet carries none of them. The contract's storage is zero at that point too.
    let total_pt = attribute_i256(state, ATTR_TOTAL_PT).unwrap_or(I256::ZERO);
    let total_sy = attribute_i256(state, ATTR_TOTAL_SY).unwrap_or(I256::ZERO);
    let last_ln_implied_rate =
        attribute_u256(state, ATTR_LAST_LN_IMPLIED_RATE)?.unwrap_or(U256::ZERO);
    let ln_fee_rate_root = attribute_u256(state, ATTR_LN_FEE_RATE_ROOT)?
        .ok_or_else(|| missing(ATTR_LN_FEE_RATE_ROOT))?;
    let reserve_fee_percent = attribute_u256(state, ATTR_RESERVE_FEE_PERCENT)?
        .ok_or_else(|| missing(ATTR_RESERVE_FEE_PERCENT))?;

    // The live index, not the stored one. `py_index_stored` is a floor that drifts below the rate
    // the contract will actually use, so quoting off it is quietly wrong between interactions.
    let (py_index, rate_sampled_at) = rate_reading(state, ATTR_PY_INDEX_CURRENT)?;

    Ok(PendleMarketState {
        market: MarketState {
            total_pt,
            total_sy,
            scalar_root,
            expiry,
            ln_fee_rate_root,
            reserve_fee_percent,
            last_ln_implied_rate,
        },
        py_index,
        rate_sampled_at,
        head_timestamp: block.timestamp,
        sy_address: address(
            statics
                .get(ATTR_SY_ADDRESS)
                .ok_or_else(|| missing(ATTR_SY_ADDRESS))?,
            ATTR_SY_ADDRESS,
        )?,
        pt_address: address(
            statics
                .get(ATTR_PT_ADDRESS)
                .ok_or_else(|| missing(ATTR_PT_ADDRESS))?,
            ATTR_PT_ADDRESS,
        )?,
        yt_address: address(
            statics
                .get(ATTR_YT_ADDRESS)
                .ok_or_else(|| missing(ATTR_YT_ADDRESS))?,
            ATTR_YT_ADDRESS,
        )?,
    })
}

fn decode_sy(
    snapshot: ComponentWithState,
    block: BlockHeader,
    all_tokens: &HashMap<Bytes, Token>,
) -> Result<PendleSyState, InvalidSnapshotError> {
    let statics = &snapshot.component.static_attributes;
    let state = &snapshot.state.attributes;

    let sy_address = Bytes::from_str(snapshot.component.id.as_str()).map_err(|e| {
        InvalidSnapshotError::ValueError(format!("SY component id is not an address: {e}"))
    })?;

    let sy_decimals = attribute_u256(statics, ATTR_SY_DECIMALS)?
        .ok_or_else(|| missing(ATTR_SY_DECIMALS))?
        .saturating_to::<u32>();
    let asset_decimals = attribute_u256(statics, ATTR_ASSET_DECIMALS)?
        .ok_or_else(|| missing(ATTR_ASSET_DECIMALS))?
        .saturating_to::<u32>();

    let (exchange_rate, rate_sampled_at) = rate_reading(state, ATTR_SY_EXCHANGE_RATE)?;
    if rate_sampled_at != NEVER_SAMPLED && exchange_rate.is_zero() {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{ATTR_SY_EXCHANGE_RATE} is zero, which no live SY reports"
        )));
    }

    // One state attribute per quotable token, named for the direction and the token. A token the
    // indexer could not classify has no attribute here and is simply not quotable. They are state
    // because an upgrade can change them.
    let tokens_in = token_classes(state, TOKEN_IN_CLASS_PREFIX)?;
    let tokens_out = token_classes(state, TOKEN_OUT_CLASS_PREFIX)?;
    if tokens_in.is_empty() && tokens_out.is_empty() {
        return Err(InvalidSnapshotError::ValueError(format!(
            "SY {sy_address} declares no quotable tokens in either direction"
        )));
    }

    // Supply bounds every redemption, and is emitted with the SY at creation.
    let total_supply =
        attribute_u256(state, SY_TOTAL_SUPPLY)?.ok_or_else(|| missing(SY_TOTAL_SUPPLY))?;
    let paused = !attribute_u256(state, PAUSED)?
        .ok_or_else(|| missing(PAUSED))?
        .is_zero();

    // Every bound on a wrap edge converts between a token amount and an SY amount, so the token's
    // own decimals are state the quote depends on. Taken from the stream's token set rather than
    // from an attribute: the indexer does not publish them, and a token it does not carry is not
    // routable anyway — such a token is simply absent here and its edges report no depth. Every
    // listed token is looked up, not only today's classified ones, since an upgrade can classify
    // another.
    let token_decimals = snapshot
        .component
        .tokens
        .iter()
        .filter_map(|token| {
            all_tokens
                .get(token)
                .map(|known| (token.clone(), known.decimals))
        })
        .collect();

    Ok(PendleSyState {
        sy_address,
        total_supply,
        paused,
        token_decimals,
        exchange_rate,
        rate_sampled_at,
        head_timestamp: block.timestamp,
        sy_decimals,
        asset_decimals,
        tokens_in,
        tokens_out,
    })
}

/// Reads a rate and the block timestamp it was read at, which the refresh emits together.
///
/// A rate that was never read decodes as zero at [`NEVER_SAMPLED`], a state that refuses to quote
/// until a delta brings the rate in. A rate without its timestamp is an error: dating it by the
/// header instead would claim it was read at a block it was not, which is exactly what the
/// exactness guard checks.
fn rate_reading(
    state: &HashMap<String, Bytes>,
    rate_name: &str,
) -> Result<(U256, u64), InvalidSnapshotError> {
    let Some(rate) = attribute_u256(state, rate_name)? else {
        return Ok((U256::ZERO, NEVER_SAMPLED));
    };
    let rate_sampled_at = attribute_u256(state, ATTR_RATE_SAMPLED_AT)?
        .ok_or_else(|| missing(ATTR_RATE_SAMPLED_AT))?
        .saturating_to::<u64>();
    Ok((rate, rate_sampled_at))
}

fn token_classes(
    statics: &HashMap<String, Bytes>,
    prefix: &str,
) -> Result<HashMap<Bytes, TokenClass>, InvalidSnapshotError> {
    let mut classes = HashMap::new();
    for (name, value) in statics {
        let Some(suffix) = name.strip_prefix(prefix) else { continue };
        let token = Bytes::from_str(suffix).map_err(|e| {
            InvalidSnapshotError::ValueError(format!("{name} does not name an address: {e}"))
        })?;
        let class = TokenClass::parse(value).ok_or_else(|| {
            InvalidSnapshotError::ValueError(format!(
                "{name} carries an unknown token class {:?}",
                String::from_utf8_lossy(value)
            ))
        })?;
        classes.insert(token, class);
    }
    Ok(classes)
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_common::{
        dto::ProtocolStateDelta,
        models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            Chain,
        },
        simulation::{
            errors::SimulationError,
            protocol_sim::{Balances, ProtocolSim},
        },
    };

    use super::*;

    const MARKET: &str = "0x34280882267ffa6383b363e278b027be083bbe3b";
    const SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
    const PT: &str = "0xb253eff1104802b97ac7e3ac9fdd73aece295a2c";
    const YT: &str = "0x04b7fa1e727d7290d6e24fa9b426d0c940283a95";
    const WSTETH: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";
    const STETH: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";
    const HEAD: u64 = 1_800_000_000;

    fn word(value: u128) -> Bytes {
        Bytes::from(
            U256::from(value)
                .to_be_bytes::<32>()
                .to_vec(),
        )
    }

    fn address_bytes(value: &str) -> Bytes {
        Bytes::from_str(value).unwrap()
    }

    fn token(value: &str) -> Token {
        Token::new(&address_bytes(value), "TKN", 18, 0, &[Some(10_000)], Chain::Ethereum, 100)
    }

    fn header() -> BlockHeader {
        BlockHeader { timestamp: HEAD, ..Default::default() }
    }

    fn snapshot(
        id: &str,
        protocol_type_name: &str,
        static_attributes: HashMap<String, Bytes>,
        attributes: HashMap<String, Bytes>,
    ) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: id.to_string(),
                attributes,
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: id.to_string(),
                protocol_type_name: protocol_type_name.to_string(),
                static_attributes,
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    /// A market as the indexer emits it on the block the factory created it: statics and fees,
    /// but no reserves and no rate yet.
    fn created_market() -> ComponentWithState {
        snapshot(
            MARKET,
            MARKET_TYPE,
            HashMap::from([
                (ATTR_SCALAR_ROOT.to_string(), word(86_364_560_000_000_000_000)),
                (ATTR_EXPIRY.to_string(), word(1_830_124_800)),
                (ATTR_SY_ADDRESS.to_string(), address_bytes(SY)),
                (ATTR_PT_ADDRESS.to_string(), address_bytes(PT)),
                (ATTR_YT_ADDRESS.to_string(), address_bytes(YT)),
            ]),
            HashMap::from([
                (ATTR_LN_FEE_RATE_ROOT.to_string(), word(499_875_041_000_000)),
                (ATTR_RESERVE_FEE_PERCENT.to_string(), word(80)),
            ]),
        )
    }

    /// An SY as the indexer emits it at creation: its classes, supply and pause flag are state.
    fn created_sy() -> ComponentWithState {
        let mut sy = snapshot(
            SY,
            SY_TYPE,
            HashMap::from([
                (ATTR_SY_DECIMALS.to_string(), word(18)),
                (ATTR_ASSET_DECIMALS.to_string(), word(18)),
            ]),
            HashMap::from([
                (
                    format!("{TOKEN_IN_CLASS_PREFIX}{}", &WSTETH[2..]),
                    Bytes::from(b"one_to_one".to_vec()),
                ),
                (
                    format!("{TOKEN_OUT_CLASS_PREFIX}{}", &WSTETH[2..]),
                    Bytes::from(b"one_to_one".to_vec()),
                ),
                (SY_TOTAL_SUPPLY.to_string(), word(1_000_000_000_000_000_000_000)),
                (PAUSED.to_string(), Bytes::from(vec![0])),
            ]),
        );
        sy.component.tokens = vec![address_bytes(SY), address_bytes(WSTETH)];
        sy
    }

    /// Supply bounds every redemption, so an SY without it cannot be decoded.
    #[tokio::test]
    async fn an_sy_without_its_supply_is_rejected() {
        let mut sy = created_sy();
        sy.state
            .attributes
            .remove(SY_TOTAL_SUPPLY);
        assert!(matches!(
            decode(sy).await,
            Err(InvalidSnapshotError::MissingAttribute(name)) if name == SY_TOTAL_SUPPLY
        ));
    }

    async fn decode(snapshot: ComponentWithState) -> Result<PendleState, InvalidSnapshotError> {
        let tokens = HashMap::from([(address_bytes(WSTETH), token(WSTETH))]);
        PendleState::try_from_with_header(
            snapshot,
            header(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::default(),
        )
        .await
    }

    fn delta(id: &str, attributes: Vec<(&str, Bytes)>) -> ProtocolStateDelta {
        ProtocolStateDelta {
            component_id: id.to_string(),
            updated_attributes: attributes
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
            deleted_attributes: Default::default(),
        }
    }

    fn one() -> BigUint {
        BigUint::from(10u64.pow(18))
    }

    #[tokio::test]
    async fn a_market_created_this_block_decodes_and_waits_for_its_rate() {
        let state = decode(created_market()).await.unwrap();

        let (max_in, max_out) = state
            .get_limits(address_bytes(SY), address_bytes(PT))
            .unwrap();
        assert_eq!((max_in, max_out), (BigUint::ZERO, BigUint::ZERO));
        assert!(matches!(
            state.get_amount_out(one(), &token(SY), &token(PT)),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    /// The rate arrives before the first liquidity does. The market is current but empty.
    #[tokio::test]
    async fn a_market_with_a_rate_but_no_liquidity_reports_no_depth() {
        let mut state = decode(created_market()).await.unwrap();
        state
            .delta_transition(
                delta(
                    MARKET,
                    vec![
                        (ATTR_PY_INDEX_CURRENT, word(1_241_884_000_000_000_000)),
                        (ATTR_RATE_SAMPLED_AT, word(HEAD.into())),
                        ("block_timestamp", word(HEAD.into())),
                    ],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        let (max_in, max_out) = state
            .get_limits(address_bytes(SY), address_bytes(PT))
            .unwrap();
        assert_eq!((max_in, max_out), (BigUint::ZERO, BigUint::ZERO));
        assert!(matches!(
            state.get_amount_out(one(), &token(SY), &token(PT)),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    #[tokio::test]
    async fn a_created_market_quotes_once_liquidity_and_a_rate_arrive() {
        let mut state = decode(created_market()).await.unwrap();
        state
            .delta_transition(
                delta(
                    MARKET,
                    vec![
                        (ATTR_TOTAL_PT, word(83_658_000_000_000_000_000)),
                        (ATTR_TOTAL_SY, word(1_429_719_000_000_000_000_000)),
                        (ATTR_LAST_LN_IMPLIED_RATE, word(20_211_000_000_000_000)),
                        (ATTR_PY_INDEX_CURRENT, word(1_241_884_000_000_000_000)),
                        (ATTR_RATE_SAMPLED_AT, word(HEAD.into())),
                        ("block_timestamp", word(HEAD.into())),
                    ],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        let (max_in, _) = state
            .get_limits(address_bytes(SY), address_bytes(PT))
            .unwrap();
        assert!(max_in > BigUint::ZERO);
        let quote = state
            .get_amount_out(one(), &token(SY), &token(PT))
            .unwrap();
        assert!(quote.amount > BigUint::ZERO);
    }

    #[tokio::test]
    async fn an_sy_whose_rate_was_never_read_quotes_only_its_one_to_one_edges() {
        let mut sy = created_sy();
        sy.state.attributes.insert(
            format!("{TOKEN_IN_CLASS_PREFIX}{}", &STETH[2..]),
            Bytes::from(b"index_rate".to_vec()),
        );
        sy.component
            .tokens
            .push(address_bytes(STETH));
        let mut state = decode(sy).await.unwrap();

        // A `one_to_one` wrap is a decimal rescale and never reads the rate.
        let quote = state
            .get_amount_out(one(), &token(WSTETH), &token(SY))
            .unwrap();
        assert_eq!(quote.amount, one(), "wstETH wraps one to one");

        let (max_in, _) = state
            .get_limits(address_bytes(STETH), address_bytes(SY))
            .unwrap();
        assert_eq!(max_in, BigUint::ZERO);
        assert!(matches!(
            state.get_amount_out(one(), &token(STETH), &token(SY)),
            Err(SimulationError::RecoverableError(_))
        ));

        state
            .delta_transition(
                delta(
                    SY,
                    vec![
                        (ATTR_SY_EXCHANGE_RATE, word(1_241_884_000_000_000_000)),
                        (ATTR_RATE_SAMPLED_AT, word(HEAD.into())),
                        ("block_timestamp", word(HEAD.into())),
                    ],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        assert!(state
            .get_amount_out(one(), &token(STETH), &token(SY))
            .is_ok());
    }

    /// The refresh emits the two together, so one without the other is a broken stream rather
    /// than a rate that has not been read yet.
    #[tokio::test]
    async fn a_rate_without_its_timestamp_is_rejected() {
        let mut market = created_market();
        market
            .state
            .attributes
            .insert(ATTR_PY_INDEX_CURRENT.to_string(), word(1_241_884_000_000_000_000));

        let error = decode(market).await.unwrap_err();

        assert!(
            matches!(&error, InvalidSnapshotError::MissingAttribute(name) if name == ATTR_RATE_SAMPLED_AT),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_read_exchange_rate_of_zero_is_rejected() {
        let mut sy = created_sy();
        sy.state
            .attributes
            .insert(ATTR_SY_EXCHANGE_RATE.to_string(), word(0));
        sy.state
            .attributes
            .insert(ATTR_RATE_SAMPLED_AT.to_string(), word(HEAD.into()));

        assert!(matches!(decode(sy).await, Err(InvalidSnapshotError::ValueError(_))));
    }
}
