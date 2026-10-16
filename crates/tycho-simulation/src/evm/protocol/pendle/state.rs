//! `PendleState` — one `ProtocolSim` covering both component types.
//!
//! Tycho binds a single state type per protocol system, but the Substreams package emits two kinds
//! of component: `pendle_market` (the SY↔PT AMM) and `pendle_sy` (the ERC-5115 wrapper). So the
//! state is an enum that dispatches on `protocol_type_name` at decode time and forwards every
//! `ProtocolSim` method.
//!
//! The six edges the brief asks for split across the two:
//!
//! | Edge | Component | Shape |
//! |---|---|---|
//! | PT → SY | market | exact, closed form |
//! | SY → PT | market | inverted by the router's search |
//! | YT → SY | market | exact, via the flash-swap identity |
//! | SY → YT | market | inverted by the router's search |
//! | underlying → SY | sy | constant rate, per token class |
//! | SY → underlying | sy | constant rate, per token class |
//!
//! The YT legs are flash-swaps against the **same reserves** as the PT legs. They are quoted on the
//! market component, and `get_limits` reports each direction's own bound rather than sharing one.
//!
//! Every market quote returns the state the trade would leave behind — the reserves it moves and
//! the implied rate the next trade is anchored on — so a caller sizing or splitting a trade prices
//! each leg against the depth the one before it left.

use std::{any::Any, collections::HashMap, fmt, fmt::Debug, str::FromStr};

use alloy::primitives::{I256, U256};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, BlockContext, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use super::{
    clock,
    math::{
        approx,
        errors::PendleError,
        market::{self, MarketState},
        pmath, sy_utils,
    },
};
use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64};

/// Gas for a swap routed through Router V4.
///
/// Measured per edge on a mainnet fork at block 25 800 000 against the wstETH market, each edge in
/// its own test on a fresh fork, then rounded up by roughly a tenth. The headroom is deliberate and
/// asymmetric in its consequences: under-estimating makes a router pick a route that costs more
/// than it was quoted, while over-estimating only makes Pendle look slightly worse than it is.
///
/// The two directions of a leg are **not** one number. Buying PT costs 40% more than selling it,
/// and buying YT 27% more than selling it, because the exact-SY-in directions run the router's
/// approximation loop on chain. Charging the dearer figure to both would misprice the cheaper
/// direction badly enough to change routing decisions.
///
/// | Edge | Measured | Charged |
/// |---|---|---|
/// | SY to PT | 299,736 | 330,000 |
/// | PT to SY | 214,123 | 235,000 |
/// | SY to YT | 385,413 | 425,000 |
/// | YT to SY | 304,325 | 335,000 |
/// | wrap, `one_to_one` | 80,206 / 77,603 | 90,000 |
/// | wrap, `index_rate` (stETH in / out) | 164,709 / 152,414 | 180,000 |
const GAS_SY_FOR_PT: u64 = 330_000;
const GAS_PT_FOR_SY: u64 = 235_000;
const GAS_SY_FOR_YT: u64 = 425_000;
const GAS_YT_FOR_SY: u64 = 335_000;

/// A `one_to_one` wrap is the cheapest case there is: the SY takes the token and mints, and does
/// nothing else.
const GAS_WRAP_ONE_TO_ONE: u64 = 90_000;

/// An `index_rate` wrap reads through to whatever protocol the SY wraps — an ERC-4626 vault, a
/// staking contract — and costs whatever that costs. Measured on the wstETH SY's stETH legs, which
/// wrap and unwrap through wstETH; another wrapped protocol can cost more or less. The class is
/// known at quote time, so the two classes are charged separately.
const GAS_WRAP_INDEX_RATE: u64 = 180_000;

/// How an SY converts a given token, as classified at component creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenClass {
    /// One token unit is one SY unit, modulo decimals. A real wrapper.
    OneToOne,
    /// Converted at the SY's exchange rate, like an ERC-4626 share.
    IndexRate,
}

impl TokenClass {
    pub fn parse(raw: &[u8]) -> Option<Self> {
        match raw {
            b"one_to_one" => Some(TokenClass::OneToOne),
            b"index_rate" => Some(TokenClass::IndexRate),
            _ => None,
        }
    }
}

/// The AMM. Everything here comes from attributes the Substreams package emits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendleMarketState {
    pub market: MarketState,
    /// `max(pyIndexStored, SY.exchangeRate())` as of [`Self::rate_sampled_at`].
    pub py_index: U256,
    /// The block timestamp at which `py_index` was read. Also the clock a quote runs on: it is
    /// the one moment for which the rate and the curve describe the same state.
    pub rate_sampled_at: u64,
    /// The chain head as the stream last reported it, injected by the decoder rather than emitted
    /// by the Substreams package. Only ever compared against `rate_sampled_at`, to tell whether
    /// this state still answers for the current block.
    pub head_timestamp: u64,
    pub sy_address: Bytes,
    pub pt_address: Bytes,
    pub yt_address: Bytes,
    /// Whether the market's SY is paused. A paused SY reverts every transfer, and every market
    /// edge moves SY, so this refuses every quote against the market.
    pub sy_paused: bool,
    /// Timestamp of the block a quote is expected to execute in, from
    /// [`ProtocolSim::apply_block`]; zero until one is given. Only ever used to judge expiry: a
    /// quote is still computed at [`Self::rate_sampled_at`], but one that would execute at or
    /// after expiry reverts on chain, so it is refused.
    #[serde(default)]
    pub execution_timestamp: u64,
}

/// The ERC-5115 wrapper, and the conversions it supports per token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendleSyState {
    pub sy_address: Bytes,
    /// Accounting-asset units per SY, as of `rate_sampled_at`.
    pub exchange_rate: U256,
    /// The block timestamp at which `exchange_rate` was read.
    pub rate_sampled_at: u64,
    /// The chain head as the stream last reported it. See [`PendleMarketState::head_timestamp`].
    pub head_timestamp: u64,
    pub sy_decimals: u32,
    pub asset_decimals: u32,
    /// Entry tokens and how each converts. A token absent here is one the indexer could not
    /// classify, and is not quotable.
    pub tokens_in: HashMap<Bytes, TokenClass>,
    /// Exit tokens and how each converts.
    pub tokens_out: HashMap<Bytes, TokenClass>,
    /// SY shares outstanding. No redemption can take more, whatever the SY holds or where: some
    /// SYs keep their yield token in a staking contract and hold none of it themselves.
    pub total_supply: U256,
    /// Whether the SY is paused, which refuses every deposit and redemption.
    pub paused: bool,
    /// Decimals of each entry and exit token, taken from the stream's token set at decode time.
    ///
    /// Every bound on a wrap edge relates an amount of the underlying token to an amount of SY,
    /// and the two are only comparable through this. A token missing here is one the stream does
    /// not carry, and its edges report no depth rather than a bound scaled by a guess.
    pub token_decimals: HashMap<Bytes, u32>,
    /// The most SY shares the SY lets exist, when it caps its own supply. A deposit that would
    /// mint past it reverts with `SupplyCapExceeded`.
    #[serde(default)]
    pub supply_cap: Option<U256>,
    /// The token the SY holds as its backing (`yieldToken()`), when the stream carries it.
    #[serde(default)]
    pub yield_token: Option<Bytes>,
    /// The SY's own balance of [`Self::yield_token`], from the component balances the stream
    /// reports. It follows transfers, so a rebasing yield token's balance lags its rebases.
    #[serde(default)]
    pub yield_token_held: Option<U256>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendleState {
    Market(PendleMarketState),
    Sy(PendleSyState),
}

impl PendleMarketState {
    /// Which of the market's three tokens an address names.
    fn role(&self, token: &Bytes) -> Option<MarketToken> {
        if *token == self.sy_address {
            Some(MarketToken::Sy)
        } else if *token == self.pt_address {
            Some(MarketToken::Pt)
        } else if *token == self.yt_address {
            Some(MarketToken::Yt)
        } else {
            None
        }
    }

    fn expired(&self) -> bool {
        market::is_expired(self.market.expiry, self.latest_known_time())
    }

    fn guard_live(&self) -> Result<(), SimulationError> {
        if self.expired() {
            return Err(PendleError::MarketExpired {
                expiry: self.market.expiry,
                block_time: self.latest_known_time(),
            }
            .into());
        }
        Ok(())
    }

    /// Refuses every edge while the SY is paused: the swap would revert moving SY. Recoverable,
    /// since an `Unpaused` brings the market back.
    fn guard_sy_unpaused(&self) -> Result<(), SimulationError> {
        if self.sy_paused {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle SY {} is paused, so its market cannot move SY",
                self.sy_address
            )));
        }
        Ok(())
    }

    /// PT in → SY out. Exact: the market has this primitive.
    fn pt_for_sy(&self, pt_in: U256) -> Result<(U256, MarketState), PendleError> {
        let executed = market::execute_trade(
            &self.market,
            self.py_index,
            -pmath::to_i256(pt_in)?,
            self.quote_time(),
        )?;
        Ok((pmath::to_u256(executed.trade.net_sy_to_account)?, executed.market))
    }

    /// YT in → SY out. A flash-swap: borrow PT, redeem PT+YT to SY, repay the market.
    ///
    /// Redeeming `n` YT needs `n` PT, so the borrowed leg is exactly the YT amount. What the
    /// trader keeps is the redemption proceeds less what repaying the borrow costs.
    fn yt_for_sy(&self, yt_in: U256) -> Result<(U256, MarketState), PendleError> {
        // The borrowed PT leg is a real market trade, so it goes through the same checks the
        // market runs: above all, it cannot lend out more PT than it holds.
        let executed = market::execute_trade(
            &self.market,
            self.py_index,
            pmath::to_i256(yt_in)?,
            self.quote_time(),
        )?;
        // Buying the PT back costs this much SY.
        let sy_owed = pmath::to_u256(-executed.trade.net_sy_to_account)?;
        // The router's callback redeems in two parts, as the contract does: just enough PY,
        // rounded up, to pay the market what it is owed, and the rest to the trader. Redeeming
        // the whole position at once and subtracting the debt after floors only once, and lands
        // a wei above what the trader receives.
        let py_to_repay = sy_utils::sy_to_asset_up(self.py_index, sy_owed)?;
        if yt_in < py_to_repay {
            return Err(PendleError::YtSaleCannotRepay {
                yt_in: yt_in.to_string(),
                py_to_repay: py_to_repay.to_string(),
            });
        }
        let sy_out = sy_utils::asset_to_sy(self.py_index, yt_in - py_to_repay)?;
        // Only the borrowed PT leg touches the market. Tokenizing and redeeming PY happens on the
        // YT contract, against reserves this component does not hold.
        Ok((sy_out, executed.market))
    }

    /// One market edge, quoted: the output, its gas, and the reserves the trade leaves behind.
    fn amount_out(
        &self,
        amount_in: U256,
        from: MarketToken,
        to: MarketToken,
    ) -> Result<MarketSwap, PendleError> {
        let (amount_out, gas, market) = match (from, to) {
            (MarketToken::Pt, MarketToken::Sy) => {
                let (out, market) = self.pt_for_sy(amount_in)?;
                (out, GAS_PT_FOR_SY, market)
            }
            (MarketToken::Yt, MarketToken::Sy) => {
                let (out, market) = self.yt_for_sy(amount_in)?;
                (out, GAS_YT_FOR_SY, market)
            }
            (MarketToken::Sy, MarketToken::Pt) => {
                let filled = approx::approx_swap_exact_sy_for_pt(
                    &self.market,
                    self.py_index,
                    amount_in,
                    self.quote_time(),
                )?;
                (filled.amount_out, GAS_SY_FOR_PT, filled.market_after)
            }
            (MarketToken::Sy, MarketToken::Yt) => {
                let filled = approx::approx_swap_exact_sy_for_yt(
                    &self.market,
                    self.py_index,
                    amount_in,
                    self.quote_time(),
                )?;
                // The search only picks how much PT to sell back. The router's callback then
                // mints PY from every SY the YT contract holds, the input plus what the PT sale
                // paid out, so the trader gets at least the guess and usually a little more.
                let net_sy_out = pmath::to_u256(filled.net_sy_to_account)?;
                let sy_to_tokenize = amount_in
                    .checked_add(net_sy_out)
                    .ok_or(PendleError::Overflow { operation: "SY to tokenize" })?;
                let yt_out = sy_utils::sy_to_asset(self.py_index, sy_to_tokenize)?;
                (yt_out, GAS_SY_FOR_YT, filled.market_after)
            }
            _ => unreachable!("unsupported pair is rejected before reaching here"),
        };
        Ok(MarketSwap { amount_out, gas, market })
    }

    /// The same state carrying the market a trade left behind.
    ///
    /// The rate and the clocks come across unchanged: a swap moves reserves, it does not advance
    /// the block or re-read the PY index, so the successor answers for the same moment as the
    /// state it came from.
    fn after(&self, market: MarketState) -> Self {
        PendleMarketState { market, ..self.clone() }
    }
}

/// What one market edge resolves to.
struct MarketSwap {
    amount_out: U256,
    gas: u64,
    /// The reserves and implied rate after the trade. A second quote against this prices at the
    /// depth the first one left, which is what makes splitting a trade across sizes meaningful.
    market: MarketState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarketToken {
    Sy,
    Pt,
    Yt,
}

impl PendleSyState {
    fn class(&self, token_in: &Bytes, token_out: &Bytes) -> Option<(TokenClass, Direction)> {
        if *token_in == self.sy_address {
            return self
                .tokens_out
                .get(token_out)
                .map(|class| (*class, Direction::Redeem));
        }
        if *token_out == self.sy_address {
            return self
                .tokens_in
                .get(token_in)
                .map(|class| (*class, Direction::Deposit));
        }
        None
    }

    /// Underlying → SY, or SY → underlying, at the class's conversion.
    ///
    /// `OneToOne` still rescales for decimals: one wstETH is one SY-wstETH, but only because both
    /// carry 18. A 6-decimal token against an 18-decimal SY is one-to-one in *units*, not in wei.
    ///
    /// A deposit is quoted at the closed form, which some SYs do not quite pay: seven of the 46
    /// live SYs swept took a 1e-7 to 1e-6 haircut on entry, so their deposits quote up to 1e-6
    /// high, and the error compounds across hops. The indexer's classifier accepts no wider a
    /// haircut, so 1e-6 bounds it. Redemptions matched their closed form to the unit.
    fn convert(
        &self,
        amount: U256,
        class: TokenClass,
        direction: Direction,
        token_decimals: u32,
    ) -> Result<U256, PendleError> {
        match (class, direction) {
            (TokenClass::OneToOne, Direction::Deposit) => {
                rescale(amount, token_decimals, self.sy_decimals)
            }
            (TokenClass::OneToOne, Direction::Redeem) => {
                rescale(amount, self.sy_decimals, token_decimals)
            }
            (TokenClass::IndexRate, Direction::Deposit) => {
                let as_asset = rescale(amount, token_decimals, self.asset_decimals)?;
                sy_utils::asset_to_sy(self.exchange_rate, as_asset)
            }
            (TokenClass::IndexRate, Direction::Redeem) => {
                let as_asset = sy_utils::sy_to_asset(self.exchange_rate, amount)?;
                rescale(as_asset, self.asset_decimals, token_decimals)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Deposit,
    Redeem,
}

fn rescale(amount: U256, from_decimals: u32, to_decimals: u32) -> Result<U256, PendleError> {
    if from_decimals == to_decimals {
        return Ok(amount);
    }
    if from_decimals < to_decimals {
        let factor = U256::from(10)
            .checked_pow(U256::from(to_decimals - from_decimals))
            .ok_or(PendleError::Overflow { operation: "rescale" })?;
        amount
            .checked_mul(factor)
            .ok_or(PendleError::Overflow { operation: "rescale" })
    } else {
        let factor = U256::from(10)
            .checked_pow(U256::from(from_decimals - to_decimals))
            .ok_or(PendleError::Overflow { operation: "rescale" })?;
        Ok(amount / factor)
    }
}

#[typetag::serde]
impl ProtocolSim for PendleState {
    /// The market's fee, as a ratio. It is a rate-space multiplier on chain, so this converts it
    /// to the amount-space number the trait documents, and it decays to zero at expiry.
    fn fee(&self) -> f64 {
        match self {
            PendleState::Market(state) => {
                let Ok(comp) = market::get_market_pre_compute(
                    &state.market,
                    state.py_index,
                    state.quote_time(),
                ) else {
                    return 0.0;
                };
                let excess = comp.fee_rate - pmath::i_one();
                let Ok(excess) = pmath::to_u256(excess) else { return 0.0 };
                u256_to_f64(excess).unwrap_or(0.0) / 1e18
            }
            // A wrapper takes no fee of its own for the classes quoted here.
            PendleState::Sy(_) => 0.0,
        }
    }

    /// The amount of `quote` needed to buy one unit of `base`.
    ///
    /// Quoted by **spending `quote` to buy `base`**, which is what the trait asks for. That
    /// direction matters here beyond arithmetic: an SY's entry and exit token lists are not the
    /// same set. SY-wstETH accepts WETH as a deposit but will not redeem to it, so pricing SY in
    /// WETH is answerable only from the buying side — asking what selling SY yields in WETH has no
    /// answer at all.
    ///
    /// Priced off a real trade rather than a closed form, so the number agrees with what a caller
    /// would actually get, fee included. One whole unit of `quote` is small against every live
    /// market's depth.
    /// The price of `base` in `quote`, from the buying side as the trait defines it: what a small
    /// trade of `quote` into `base` pays per unit.
    ///
    /// Not every component trades both ways. An SY can accept a token it never pays out, and a
    /// redemption the SY holds nothing for has no depth, while the other direction trades. When
    /// buying `base` does not quote, the price comes from selling it instead, as `quote` received
    /// per unit of `base`. A wrap edge charges no fee, so the two sides give the same price.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        match self.marginal_rate(quote, base) {
            Ok(base_per_quote) => Ok(1.0 / base_per_quote),
            Err(buying) => self
                .marginal_rate(base, quote)
                .map_err(|_| buying),
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if amount_in == BigUint::from(0u32) {
            return Err(SimulationError::InvalidInput("amount_in is zero".to_string(), None));
        }
        // More than any token's supply. The math is checked, but an amount this size overflows
        // inside it, and an overflow there reads as a broken state rather than a request too large.
        if amount_in > BigUint::from(u128::MAX) {
            return Err(SimulationError::InvalidInput(
                format!("amount_in {amount_in} exceeds any token supply"),
                None,
            ));
        }
        let amount = biguint_to_u256(&amount_in);

        let (out, gas, new_state) = match self {
            PendleState::Market(state) => {
                state.guard_live()?;
                state.guard_sy_unpaused()?;
                state.guard_current()?;
                let from = state
                    .role(&token_in.address)
                    .ok_or_else(|| unknown_pair(token_in, token_out))?;
                let to = state
                    .role(&token_out.address)
                    .ok_or_else(|| unknown_pair(token_in, token_out))?;
                // PT↔YT never touches the market: minting and redeeming PY is the YT's business,
                // and SY is one side of every market edge.
                if from == to || (from != MarketToken::Sy && to != MarketToken::Sy) {
                    return Err(unknown_pair(token_in, token_out));
                }
                let swap = state.amount_out(amount, from, to)?;
                (swap.amount_out, swap.gas, PendleState::Market(state.after(swap.market)))
            }
            PendleState::Sy(state) => {
                let (class, direction) = state
                    .class(&token_in.address, &token_out.address)
                    .ok_or_else(|| unquotable_pair(token_in, token_out))?;
                state.guard_quotable(class)?;
                let token_decimals = match direction {
                    Direction::Deposit => token_in.decimals,
                    Direction::Redeem => token_out.decimals,
                };
                let gas = match class {
                    TokenClass::OneToOne => GAS_WRAP_ONE_TO_ONE,
                    TokenClass::IndexRate => GAS_WRAP_INDEX_RATE,
                };
                if direction == Direction::Redeem {
                    let max_in = state.max_redeem_sy(&token_out.address, class, token_decimals)?;
                    if amount > max_in {
                        return Err(SimulationError::InvalidInput(
                            format!(
                                "redeeming {amount} SY exceeds the {max_in} the SY can pay out"
                            ),
                            None,
                        ));
                    }
                }
                let out = state.convert(amount, class, direction, token_decimals)?;
                if direction == Direction::Deposit {
                    if let Some(room) = state.deposit_headroom() {
                        if out > room {
                            return Err(SimulationError::InvalidInput(
                                format!(
                                    "minting {out} SY passes the supply cap, which leaves {room}"
                                ),
                                None,
                            ));
                        }
                    }
                }
                let token = match direction {
                    Direction::Deposit => &token_in.address,
                    Direction::Redeem => &token_out.address,
                };
                let after = state.after(direction, token, amount, out)?;
                (out, gas, PendleState::Sy(after))
            }
        };

        Ok(GetAmountOutResult {
            amount: u256_to_biguint(out),
            gas: BigUint::from(gas),
            new_state: Box::new(new_state),
        })
    }

    /// The largest trade that will not revert, per direction.
    ///
    /// Each market direction has its own binding constraint and they do not follow from one
    /// another: SY→PT is bounded by the rate floor, PT→SY by the 96% proportion cap, and the YT
    /// legs by the same cap mapped through the flash-swap identity. The SY→PT and SY→YT bounds are
    /// obtained by inverting the router's own search bound, so the reported input is one the search
    /// actually fills.
    ///
    /// A pair this component does not trade — PT against YT, or a token that is not one of its
    /// three — reports **zero depth rather than an error**. A caller enumerating the pairs of a
    /// component is asking a question, and "none" is the answer; only an actual swap request is
    /// wrong enough to fail. The integration test enumerates every pair, PT↔YT included.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        match self {
            PendleState::Market(state) => {
                if state.expired() {
                    // Dead, not merely empty. Zero depth in both directions.
                    return Ok(no_depth());
                }
                // Every edge moves SY, which a paused SY refuses to transfer.
                if state.sy_paused {
                    return Ok(no_depth());
                }
                // A bound this state cannot quote against is not a bound. Reported as no depth
                // rather than an error, so a router skips the market the way it skips an expired
                // one instead of treating a routine gap as a failure.
                if !state.is_current() {
                    return Ok(no_depth());
                }
                // No liquidity yet. Every bound below divides by the reserves.
                if state.market.total_pt <= I256::ZERO || state.market.total_sy <= I256::ZERO {
                    return Ok(no_depth());
                }
                let (Some(from), Some(to)) = (state.role(&sell_token), state.role(&buy_token))
                else {
                    return Ok(no_depth());
                };

                // Each limit is the largest input the edge actually fills: a closed form for the
                // boundary, checked against the quote, which replicates the router to the wei.
                let fills = |amount: U256| {
                    state
                        .amount_out(amount, from, to)
                        .is_ok()
                };
                let max_in = match (from, to) {
                    // The router's search stops at the haircut rate floor, and accepts a request
                    // up to `eps` above what that bound costs.
                    (MarketToken::Sy, MarketToken::Pt) => {
                        let (exact, _) = approx::max_sy_in_for_pt(
                            &state.market,
                            state.py_index,
                            state.quote_time(),
                        )?;
                        largest_fill(approx::with_eps_slack(exact)?, exact, fills)
                    }
                    // The same search, bounded by the proportion cap the minted PT is sold into.
                    (MarketToken::Sy, MarketToken::Yt) => {
                        let (exact, _) = approx::max_sy_in_for_yt(
                            &state.market,
                            state.py_index,
                            state.quote_time(),
                        )?;
                        largest_fill(approx::with_eps_slack(exact)?, exact, fills)
                    }
                    // Selling PT hands PT *to* the market, so the 96% proportion cap binds, and the
                    // market checks it directly: the closed form is the boundary.
                    (MarketToken::Pt, MarketToken::Sy) => state.max_pt_in()?,
                    // Selling YT flash-*borrows* PT from the market, outside the router's search,
                    // so the market's own rate floor binds without the search's haircut. Close to
                    // expiry, repaying the borrowed PT can bind first; the walk finds whichever.
                    (MarketToken::Yt, MarketToken::Sy) => {
                        largest_fill(state.max_pt_out_unhaircut()?, state.max_pt_out()?, fills)
                    }
                    // PT against YT, or either against itself: not an edge of this market.
                    _ => return Ok(no_depth()),
                };
                let max_out = state
                    .amount_out(max_in, from, to)?
                    .amount_out;
                Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
            }
            PendleState::Sy(state) => {
                let Some((class, direction)) = state.class(&sell_token, &buy_token) else {
                    return Ok(no_depth());
                };
                // As for a market: a rate this state cannot quote with bounds nothing. A
                // `one_to_one` edge never reads the rate, so only a paused SY stops it.
                if state.guard_quotable(class).is_err() {
                    return Ok(no_depth());
                }
                // The underlying token's own decimals, never the SY's. Both bounds below relate a
                // token amount to an SY amount, and substituting `sy_decimals` makes each wrong by
                // `10^|difference|` on any pair whose decimals do not already match.
                let token = match direction {
                    Direction::Deposit => &sell_token,
                    Direction::Redeem => &buy_token,
                };
                let Some(token_decimals) = state.token_decimals.get(token).copied() else {
                    return Ok(no_depth());
                };
                let convert =
                    |amount: U256| state.convert(amount, class, direction, token_decimals);
                let max_in = match direction {
                    // A capped SY mints only up to its cap. An uncapped one has no limit of its
                    // own; what the protocol it wraps accepts is not visible here.
                    Direction::Deposit => {
                        let unbounded = unbounded_soft_limit();
                        match state.deposit_headroom() {
                            None => unbounded,
                            Some(room) => largest_input_within(room, unbounded, convert)?,
                        }
                    }
                    Direction::Redeem => state.max_redeem_sy(token, class, token_decimals)?,
                };
                let max_out = convert(max_in)?;
                // An input too small to produce anything is no depth, not a trade.
                if max_out.is_zero() {
                    return Ok(no_depth());
                }
                Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
            }
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        match self {
            PendleState::Market(state) => state.apply(&delta),
            // An SY's redemption to its yield token is bounded by what it holds of it, which
            // arrives as a component balance.
            PendleState::Sy(state) => {
                state.apply(&delta)?;
                state.apply_balances(balances);
                Ok(())
            }
        }
    }

    /// Records the execution block's timestamp on a market, so a quote that would execute at or
    /// after expiry is refused. Re-emits only when that flips the market to expired; an SY has no
    /// expiry and never changes.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let PendleState::Market(state) = self else { return false };
        if state.execution_timestamp == block.timestamp() {
            return false;
        }
        let expired_before = state.expired();
        state.execution_timestamp = block.timestamp();
        expired_before != state.expired()
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<PendleState>()
            .is_some_and(|other| self == other)
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }
}

impl PendleState {
    /// Units of `token_out` a small trade returns per unit of `token_in`.
    ///
    /// Probes with a thousandth of the depth, capped at one unit: small enough to stay inside the
    /// limit on a shallow market and to leave out most of the price impact, large enough that the
    /// output is not lost to rounding. A direction reporting no depth is probed with one unit and
    /// answers if the quote does: an SY's conversion rate exists even when it holds nothing to pay.
    fn marginal_rate(&self, token_in: &Token, token_out: &Token) -> Result<f64, SimulationError> {
        let one_unit = BigUint::from(10u32).pow(token_in.decimals);
        let (max_in, _) = self.get_limits(token_in.address.clone(), token_out.address.clone())?;
        let within_depth = max_in / 1000u32;
        let probe =
            if within_depth == BigUint::from(0u32) { one_unit } else { one_unit.min(within_depth) };
        let quoted = self.get_amount_out(probe.clone(), token_in, token_out)?;
        let amount_in = biguint_to_f64(&probe) / 10f64.powi(token_in.decimals as i32);
        let amount_out = biguint_to_f64(&quoted.amount) / 10f64.powi(token_out.decimals as i32);
        if amount_out == 0.0 {
            return Err(SimulationError::FatalError(format!(
                "{probe} of {} returns no {}, so there is no rate",
                token_in.address, token_out.address
            )));
        }
        Ok(amount_out / amount_in)
    }
}

impl PendleMarketState {
    /// The timestamp this market's quotes are evaluated at: the block its rate was read at.
    ///
    /// Not the chain head. Evaluating the curve at the head while holding a rate from an earlier
    /// block would pair two different moments — see [`super::clock`].
    pub fn quote_time(&self) -> u64 {
        self.rate_sampled_at
    }

    /// Whether this state still answers for the current block.
    pub fn is_current(&self) -> bool {
        clock::is_current(self.rate_sampled_at, self.head_timestamp)
    }

    /// The latest moment this state has evidence of, from either clock.
    ///
    /// Expiry is judged against this rather than against [`Self::quote_time`], which is the rate's
    /// own date. A rate that stopped resolving before the market expired would otherwise leave
    /// `quote_time` frozen below `expiry` and the market never seen to die. The head is a real
    /// chain timestamp, so taking the later of the two can only move the verdict up to where the
    /// chain already is — never past it, and never onto a market that still trades.
    ///
    /// The execution block counts too. A quote taken at the last block before expiry executes in
    /// the next one, where the market reverts with `MarketExpired`.
    fn latest_known_time(&self) -> u64 {
        self.rate_sampled_at
            .max(self.head_timestamp)
            .max(self.execution_timestamp)
    }

    fn guard_current(&self) -> Result<(), SimulationError> {
        if !self.is_current() {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle market rate was read at {} but the chain head is {}; only a quote at the \
                 sampled block is exact",
                self.rate_sampled_at, self.head_timestamp
            )));
        }
        Ok(())
    }

    /// The most PT the market will absorb, from the 96% proportion cap.
    fn max_pt_in(&self) -> Result<U256, PendleError> {
        let comp = market::get_market_pre_compute(&self.market, self.py_index, self.quote_time())?;
        approx::calc_soft_max_pt_in(&self.market, &comp)
    }

    /// The most PT the router's search will take out, from the exchange-rate floor less its
    /// 999/1000 haircut.
    fn max_pt_out(&self) -> Result<U256, PendleError> {
        let comp = market::get_market_pre_compute(&self.market, self.py_index, self.quote_time())?;
        approx::calc_max_pt_out(&comp, self.market.total_pt)
    }

    /// The most PT the market itself will send out before its exchange rate falls below par.
    fn max_pt_out_unhaircut(&self) -> Result<U256, PendleError> {
        let comp = market::get_market_pre_compute(&self.market, self.py_index, self.quote_time())?;
        approx::calc_max_pt_out_unhaircut(&comp, self.market.total_pt)
    }
}

impl PendleSyState {
    /// Whether this wrapper still answers for the current block.
    pub fn is_current(&self) -> bool {
        clock::is_current(self.rate_sampled_at, self.head_timestamp)
    }

    /// Refuses a quote this state cannot answer: any edge of a paused SY, and an `index_rate`
    /// edge whose rate the chain has moved past. A `one_to_one` edge is a decimal rescale that
    /// never reads the rate, so its clock does not matter.
    fn guard_quotable(&self, class: TokenClass) -> Result<(), SimulationError> {
        if self.paused {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle SY {} is paused",
                self.sy_address
            )));
        }
        if class == TokenClass::IndexRate && !self.is_current() {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle SY rate was read at {} but the chain head is {}; only a quote at the \
                 sampled block is exact",
                self.rate_sampled_at, self.head_timestamp
            )));
        }
        Ok(())
    }

    /// The most SY a redemption to `token` can take.
    ///
    /// Supply bounds every exit: no more shares can be burned than exist. On a mainnet fork, 82 of
    /// the 88 exits of live SYs redeemed their whole supply to the wei. An exit to the SY's own
    /// yield token is also paid out of what the SY holds of it, so the payout cannot pass that
    /// holding; one live SY held 28 wei less than its supply needed. The holding counts only when
    /// it backs at least 999/1000 of the whole supply's payout: an SY that keeps its yield
    /// token in a staking contract holds little of it and pays from there.
    ///
    /// An exit through a protocol the SY wraps can be limited further, by that protocol's own
    /// liquidity or pause, which nothing here can see.
    fn max_redeem_sy(
        &self,
        token: &Bytes,
        class: TokenClass,
        token_decimals: u32,
    ) -> Result<U256, PendleError> {
        let supply = self.total_supply;
        let (Some(yield_token), Some(held)) = (&self.yield_token, self.yield_token_held) else {
            return Ok(supply);
        };
        if token != yield_token {
            return Ok(supply);
        }
        let convert = |amount: U256| self.convert(amount, class, Direction::Redeem, token_decimals);
        let payout = convert(supply)?;
        if held >= payout || held < payout - payout / U256::from(1000) {
            return Ok(supply);
        }
        largest_input_within(held, supply, convert)
    }

    /// The SY a deposit can still mint before the supply cap; `None` for an uncapped SY.
    fn deposit_headroom(&self) -> Option<U256> {
        self.supply_cap
            .map(|cap| cap.saturating_sub(self.total_supply))
    }

    /// The SY's balance of a component token, from the stream's component balances, keyed by the
    /// component id the indexer gives an SY: its address.
    fn apply_balances(&mut self, balances: &Balances) {
        let (Some(yield_token), Some(component)) = (
            &self.yield_token,
            balances
                .component_balances
                .get(&self.sy_address.to_string()),
        ) else {
            return;
        };
        if let Some(balance) = component.get(yield_token) {
            self.yield_token_held = Some(U256::from_be_slice(balance));
        }
    }

    /// The same wrapper after a conversion: a deposit mints the SY it returns, a redemption burns
    /// the SY it takes.
    ///
    /// What it holds of its yield token moves too: a deposit of it adds to the holding, and a
    /// redemption to it pays out of it.
    fn after(
        &self,
        direction: Direction,
        token: &Bytes,
        amount_in: U256,
        amount_out: U256,
    ) -> Result<Self, PendleError> {
        let mut next = self.clone();
        if self.yield_token.as_ref() == Some(token) {
            next.yield_token_held = self
                .yield_token_held
                .map(|held| match direction {
                    Direction::Deposit => held.saturating_add(amount_in),
                    Direction::Redeem => held.saturating_sub(amount_out),
                });
        }
        next.total_supply = match direction {
            Direction::Deposit => self
                .total_supply
                .checked_add(amount_out)
                .ok_or(PendleError::Overflow { operation: "SY supply after a deposit" })?,
            Direction::Redeem => self
                .total_supply
                .checked_sub(amount_in)
                .ok_or(PendleError::Overflow { operation: "SY supply after a redemption" })?,
        };
        Ok(next)
    }
}

fn biguint_to_f64(value: &BigUint) -> f64 {
    value
        .to_string()
        .parse::<f64>()
        .unwrap_or(f64::INFINITY)
}

fn unknown_pair(token_in: &Token, token_out: &Token) -> SimulationError {
    SimulationError::FatalError(format!(
        "{} -> {} is not an edge of this Pendle component",
        token_in.address, token_out.address
    ))
}

fn unquotable_pair(token_in: &Token, token_out: &Token) -> SimulationError {
    SimulationError::FatalError(format!(
        "{} -> {} is not a quotable conversion for this SY: the indexer could not classify it, so \
         no closed form is known",
        token_in.address, token_out.address
    ))
}

/// The soft limit this repo uses for an edge with no reserve behind it, matching
/// `native_wrapper`. Large enough not to constrain any real trade, small enough that a caller can
/// multiply by it without overflowing.
///
/// An uncapped SY's deposit reports it. Depositing the SY's own yield token has no limit at all:
/// on a mainnet fork, 51 of 53 such deposits took a trillion whole tokens. A deposit converted
/// through the protocol the SY wraps can be limited there (17 of 47 were, by an ERC-4626
/// `maxDeposit`, Lido's stake limit and others), which nothing here can see.
fn unbounded_soft_limit() -> U256 {
    U256::from(u128::MAX)
}

/// The largest input a market edge fills, found from `candidate`, a closed form for the edge's
/// boundary.
///
/// The closed forms land within a few wei of the boundary, except where the contract's own
/// estimate rounds (selling YT, to about 1e-11 of the amount). If the edge fills at `candidate`, a
/// few wei above it are tried; if it does not, it is walked down in doubling steps until it does,
/// which lands within twice the miss. `fallback`, a bound the edge is known to fill, covers a
/// candidate that misses by more than a thousandth, which no tested state does.
fn largest_fill(candidate: U256, fallback: U256, fills: impl Fn(U256) -> bool) -> U256 {
    if fills(candidate) {
        let mut largest = candidate;
        for _ in 0..4 {
            let next = largest + U256::from(1);
            if !fills(next) {
                break;
            }
            largest = next;
        }
        return largest;
    }
    let mut step = U256::from(1);
    while step <= candidate / U256::from(1000) {
        if fills(candidate - step) {
            return candidate - step;
        }
        step <<= 1;
    }
    fallback
}

/// The largest input in `[0, upper]` whose conversion stays at or under `target`.
///
/// A wrap conversion is monotonic and plain integer arithmetic, so a bisection over it costs a few
/// hundred multiplications.
fn largest_input_within(
    target: U256,
    upper: U256,
    convert: impl Fn(U256) -> Result<U256, PendleError>,
) -> Result<U256, PendleError> {
    if convert(upper)? <= target {
        return Ok(upper);
    }
    let (mut within, mut past) = (U256::ZERO, upper);
    while past - within > U256::from(1) {
        let middle = within + (past - within) / U256::from(2);
        if convert(middle)? <= target {
            within = middle;
        } else {
            past = middle;
        }
    }
    Ok(within)
}

/// What `get_limits` reports for a pair this component cannot trade.
fn no_depth() -> (BigUint, BigUint) {
    (BigUint::from(0u32), BigUint::from(0u32))
}

/// Reads an attribute as a big-endian unsigned integer.
pub(super) fn attribute_u256(
    attributes: &HashMap<String, Bytes>,
    name: &str,
) -> Result<Option<U256>, ValueTooWide> {
    attributes
        .get(name)
        .map(|raw| u256_from_be(raw, name))
        .transpose()
}

/// A big-endian value that does not fit in 256 bits.
#[derive(Debug)]
pub(super) struct ValueTooWide {
    name: String,
    len: usize,
}

impl fmt::Display for ValueTooWide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is {} bytes, more than a 256-bit value holds", self.name, self.len)
    }
}

impl From<ValueTooWide> for TransitionError {
    fn from(error: ValueTooWide) -> Self {
        TransitionError::DecodeError(error.to_string())
    }
}

/// Reads a big-endian unsigned value, which may carry leading zero bytes past 32: the indexer
/// writes unsigned values with `to_signed_bytes_be`, so anything at or above 2^255 arrives as 33
/// bytes with a leading zero.
pub(super) fn u256_from_be(raw: &[u8], name: &str) -> Result<U256, ValueTooWide> {
    let significant = &raw[raw
        .iter()
        .take_while(|byte| **byte == 0)
        .count()..];
    if significant.len() > 32 {
        return Err(ValueTooWide { name: name.to_string(), len: raw.len() });
    }
    Ok(U256::from_be_slice(significant))
}

/// Reads an attribute as a big-endian signed integer, `None` when it is absent.
///
/// The indexer writes minimal two's complement, which fits any 256-bit value in 32 bytes, so a
/// wider value is an error rather than something to truncate.
pub(super) fn attribute_i256(
    attributes: &HashMap<String, Bytes>,
    name: &str,
) -> Result<Option<I256>, ValueTooWide> {
    let Some(raw) = attributes.get(name) else { return Ok(None) };
    if raw.len() > 32 {
        return Err(ValueTooWide { name: name.to_string(), len: raw.len() });
    }
    let mut padded = [0u8; 32];
    let start = 32 - raw.len();
    padded[start..].copy_from_slice(raw);
    // Sign-extend, since the indexer writes minimal two's complement.
    if raw
        .first()
        .is_some_and(|b| b & 0x80 != 0)
    {
        for byte in padded.iter_mut().take(start) {
            *byte = 0xff;
        }
    }
    Ok(Some(I256::from_be_bytes(padded)))
}

impl PendleMarketState {
    fn apply(&mut self, delta: &ProtocolStateDelta) -> Result<(), TransitionError> {
        let attributes = &delta.updated_attributes;
        if let Some(value) = attribute_i256(attributes, "total_pt")? {
            self.market.total_pt = value;
        }
        if let Some(value) = attribute_i256(attributes, "total_sy")? {
            self.market.total_sy = value;
        }
        if let Some(value) = attribute_u256(attributes, "last_ln_implied_rate")? {
            self.market.last_ln_implied_rate = value;
        }
        if let Some(value) = attribute_u256(attributes, "ln_fee_rate_root")? {
            self.market.ln_fee_rate_root = value;
        }
        if let Some(value) = attribute_u256(attributes, "reserve_fee_percent")? {
            self.market.reserve_fee_percent = value;
        }
        if let Some(value) = attribute_u256(attributes, "py_index_current")? {
            self.py_index = value;
        }
        if let Some(value) = attribute_u256(attributes, "rate_sampled_at")? {
            self.rate_sampled_at = value.saturating_to();
        }
        if let Some(value) = attribute_u256(attributes, SY_PAUSED)? {
            self.sy_paused = !value.is_zero();
        }
        // The Substreams refresh emits this on every refresh block whether or not the rate
        // resolved, which is what lets a delta move the head at all — a `ProtocolStateDelta`
        // carries no header. It therefore advances only as often as `sy_rate_refresh_blocks`
        // allows, so the staleness guard under-fires between refreshes at any coarser setting.
        if let Some(value) = attribute_u256(attributes, "block_timestamp")? {
            self.head_timestamp = value.saturating_to();
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

impl PendleSyState {
    fn apply(&mut self, delta: &ProtocolStateDelta) -> Result<(), TransitionError> {
        let attributes = &delta.updated_attributes;
        if let Some(value) = attribute_u256(attributes, "sy_exchange_rate")? {
            // The decoder rejects a zero rate too. Every conversion divides by it.
            if value.is_zero() {
                return Err(TransitionError::DecodeError(format!(
                    "SY {} reported an exchange rate of zero",
                    self.sy_address
                )));
            }
            self.exchange_rate = value;
        }
        if let Some(value) = attribute_u256(attributes, "rate_sampled_at")? {
            self.rate_sampled_at = value.saturating_to();
        }
        if let Some(value) = attribute_u256(attributes, "block_timestamp")? {
            self.head_timestamp = value.saturating_to();
        }
        if let Some(value) = attribute_u256(attributes, SY_TOTAL_SUPPLY)? {
            self.total_supply = value;
        }
        if let Some(value) = attribute_u256(attributes, PAUSED)? {
            self.paused = !value.is_zero();
        }
        if let Some(value) = attribute_u256(attributes, SY_SUPPLY_CAP)? {
            self.supply_cap = Some(value);
        }
        if delta
            .deleted_attributes
            .contains(SY_SUPPLY_CAP)
        {
            self.supply_cap = None;
        }
        // An upgrade re-classifies the SY's tokens: each class arrives as an update, and a token
        // that no longer matches either closed form loses its class as a deletion.
        for (name, value) in attributes {
            let Some((direction, token)) =
                class_attribute_token(name).map_err(TransitionError::DecodeError)?
            else {
                continue;
            };
            let class = TokenClass::parse(value).ok_or_else(|| {
                TransitionError::DecodeError(format!(
                    "{name} carries an unknown token class {:?}",
                    String::from_utf8_lossy(value)
                ))
            })?;
            self.classes_mut(direction)
                .insert(token, class);
        }
        for name in &delta.deleted_attributes {
            let Some((direction, token)) =
                class_attribute_token(name).map_err(TransitionError::DecodeError)?
            else {
                continue;
            };
            self.classes_mut(direction)
                .remove(&token);
        }
        Ok(())
    }

    fn classes_mut(&mut self, direction: Direction) -> &mut HashMap<Bytes, TokenClass> {
        match direction {
            Direction::Deposit => &mut self.tokens_in,
            Direction::Redeem => &mut self.tokens_out,
        }
    }
}

/// State attribute: SY shares outstanding.
pub(super) const SY_TOTAL_SUPPLY: &str = "sy_total_supply";
/// State attribute: whether the SY is paused.
pub(super) const PAUSED: &str = "paused";
/// State attribute on a market: whether its SY is paused.
pub(super) const SY_PAUSED: &str = "sy_paused";
/// State attribute: the cap on an SY's own supply, absent when it has none.
pub(super) const SY_SUPPLY_CAP: &str = "sy_supply_cap";
/// Static attribute: the token an SY holds as its backing.
pub(super) const YIELD_TOKEN: &str = "yield_token";
/// Prefixes of the per-token class attributes, followed by the token's hex address.
pub(super) const TOKEN_IN_CLASS_PREFIX: &str = "token_in_class_0x";
pub(super) const TOKEN_OUT_CLASS_PREFIX: &str = "token_out_class_0x";

/// The direction and token a class attribute names, or `None` for any other attribute.
///
/// A class attribute whose suffix is not a 20-byte address is an error, the same for a snapshot
/// and a delta, rather than an attribute to skip: skipping it would leave a class the indexer
/// meant to set or delete unapplied.
pub(super) fn class_attribute_token(name: &str) -> Result<Option<(Direction, Bytes)>, String> {
    let (direction, suffix) = if let Some(suffix) = name.strip_prefix(TOKEN_IN_CLASS_PREFIX) {
        (Direction::Deposit, suffix)
    } else if let Some(suffix) = name.strip_prefix(TOKEN_OUT_CLASS_PREFIX) {
        (Direction::Redeem, suffix)
    } else {
        return Ok(None);
    };
    let token = Bytes::from_str(&format!("0x{suffix}"))
        .map_err(|e| format!("{name} does not name an address: {e}"))?;
    if token.len() != 20 {
        return Err(format!("{name} names {} bytes, not a 20-byte address", token.len()));
    }
    Ok(Some((direction, token)))
}
