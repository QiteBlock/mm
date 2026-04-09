use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Duration, Timelike, Utc};
use rust_decimal::{Decimal, MathematicalOps};

use crate::{
    config::{AppConfig, PairConfig, ParsedConfig, PriceSource},
    domain::{FactorSnapshot, MarketRegime, RecentTrade},
    state::BotState,
};

#[derive(Default)]
pub struct FactorEngine {
    symbol_state: HashMap<String, SymbolFactorState>,
}

#[derive(Default)]
struct SymbolFactorState {
    ewma_mean: Option<Decimal>,
    ewma_variance: Decimal,
    last_mid: Option<Decimal>,
    last_mid_timestamp: Option<DateTime<Utc>>,
    vol_history: VecDeque<Decimal>,
    smoothed_flow: Decimal,
    regime: MarketRegime,
    /// Continuous [0, 1] intensity of current regime (S5).
    regime_intensity: Decimal,
    /// When the current regime was entered (S5 dwell time).
    regime_entered_at: Option<DateTime<Utc>>,
    trade_velocity_ewma: Decimal,
    bbo_spread_ewma: Option<Decimal>,
}

impl FactorEngine {
    pub fn compute(
        &mut self,
        config: &AppConfig,
        parsed: &ParsedConfig,
        pair: &PairConfig,
        state: &BotState,
    ) -> Option<FactorSnapshot> {
        let market = state.market.symbols.get(&pair.symbol)?;
        let mid = market.mid_price()?;
        let now = market.last_updated?;
        let symbol_state = self.symbol_state.entry(pair.symbol.clone()).or_default();
        let alpha = parsed.factors.volatility_ewma_alpha;

        if let Some(prev_mid) = symbol_state.last_mid {
            if prev_mid > Decimal::ZERO && mid > Decimal::ZERO {
                let log_return = (mid / prev_mid).ln();
                let mean_prev = symbol_state.ewma_mean.unwrap_or(log_return);
                let mean = alpha * log_return + (Decimal::ONE - alpha) * mean_prev;
                let diff = log_return - mean_prev;
                let variance =
                    alpha * diff * diff + (Decimal::ONE - alpha) * symbol_state.ewma_variance;
                symbol_state.ewma_mean = Some(mean);
                symbol_state.ewma_variance = variance.max(Decimal::ZERO);
                symbol_state.vol_history.push_back(log_return.abs());
                while symbol_state.vol_history.len() > 512 {
                    symbol_state.vol_history.pop_front();
                }
            }
        }

        symbol_state.last_mid = Some(mid);
        symbol_state.last_mid_timestamp = Some(now);

        let volatility = symbol_state.ewma_variance.sqrt().unwrap_or(Decimal::ZERO);
        let volume_window = Duration::seconds(config.factors.volume_window_secs);
        let volume_window_start = now - volume_window;
        let n_trade_window = Duration::seconds(config.factors.n_trade_window_secs);
        let n_trade_window_start = now - n_trade_window;

        let recent_trades: Vec<&RecentTrade> = market
            .recent_trades
            .iter()
            .filter(|trade| trade.timestamp >= volume_window_start)
            .collect();
        let rolling_volume: Decimal = recent_trades.iter().map(|trade| trade.quantity).sum();
        let buy_volume: Decimal = recent_trades
            .iter()
            .filter(|trade| matches!(trade.taker_side, Some(crate::domain::Side::Bid)))
            .map(|trade| trade.quantity)
            .sum();
        let sell_volume: Decimal = recent_trades
            .iter()
            .filter(|trade| matches!(trade.taker_side, Some(crate::domain::Side::Ask)))
            .map(|trade| trade.quantity)
            .sum();
        let recent_trade_count = market
            .recent_trades
            .iter()
            .filter(|trade| trade.timestamp >= n_trade_window_start)
            .count();

        // S6: raw flow direction, signed [-1, 1], independent of volume magnitude.
        let flow_total = buy_volume + sell_volume;
        let raw_flow_direction = if flow_total.is_zero() {
            Decimal::ZERO
        } else {
            (buy_volume - sell_volume) / flow_total
        };
        let flow_alpha = parsed
            .factors
            .flow_imbalance_weight
            .max(Decimal::new(1, 2))
            .min(Decimal::ONE);
        symbol_state.smoothed_flow =
            flow_alpha * raw_flow_direction + (Decimal::ONE - flow_alpha) * symbol_state.smoothed_flow;

        if let Some(spread_bps) = market.bbo_spread_bps() {
            symbol_state.bbo_spread_ewma = Some(match symbol_state.bbo_spread_ewma {
                Some(previous) => Decimal::new(2, 1) * spread_bps + Decimal::new(8, 1) * previous,
                None => spread_bps,
            });
        }

        let velocity_window_secs = config.factors.n_trade_window_secs.max(1);
        let trade_velocity =
            Decimal::from(recent_trade_count as u64) / Decimal::from(velocity_window_secs as u64);
        let velocity_alpha = parsed
            .factors
            .trade_velocity_alpha
            .max(Decimal::new(1, 2))
            .min(Decimal::ONE);
        symbol_state.trade_velocity_ewma = if symbol_state.trade_velocity_ewma.is_zero() {
            trade_velocity
        } else {
            velocity_alpha * trade_velocity
                + (Decimal::ONE - velocity_alpha) * symbol_state.trade_velocity_ewma
        };
        let trade_velocity_ratio = if symbol_state.trade_velocity_ewma.is_zero() {
            Decimal::ONE
        } else {
            trade_velocity / symbol_state.trade_velocity_ewma
        };

        let cross_flow = cross_symbol_flow(
            &pair.symbol,
            state,
            volume_window_start,
            parsed.factors.cross_symbol_vol_weight,
        );
        let bbo_spread_component = symbol_state
            .bbo_spread_ewma
            .unwrap_or_else(|| market.bbo_spread_bps().unwrap_or(Decimal::ZERO))
            * parsed.factors.bbo_spread_vol_weight;

        // S3: Vol-independent, convex inventory skew.
        // Formula: pos_ratio * (1 + convexity * |pos_ratio|) * effective_base
        // where effective_base = vol_floor * inventory_risk_constant (preserves existing scale).
        let pair_parsed = parsed.pair(&pair.symbol)?;
        let current_position = state
            .positions
            .get(&pair.symbol)
            .map(|position| position.quantity)
            .unwrap_or(Decimal::ZERO);
        let pos_ratio = if pair_parsed.max_position_base.is_zero() {
            Decimal::ZERO
        } else {
            current_position / pair_parsed.max_position_base
        };
        let abs_ratio = pos_ratio.abs();
        let convexity = parsed.factors.inventory_skew_convexity;
        let effective_skew_base =
            parsed.factors.volatility_floor * parsed.factors.inventory_risk_constant;
        let inventory_skew =
            pos_ratio * (Decimal::ONE + convexity * abs_ratio) * effective_skew_base;

        // S5: Continuous regime intensity + minimum dwell time.
        let flow_abs = (symbol_state.smoothed_flow + cross_flow).abs();
        let target_regime =
            candidate_regime(flow_abs, volatility, trade_velocity_ratio, trade_velocity);
        let min_dwell = Duration::seconds(parsed.factors.regime_min_dwell_secs as i64);
        let can_change = symbol_state
            .regime_entered_at
            .map(|t| now - t >= min_dwell)
            .unwrap_or(true);
        if can_change && target_regime != symbol_state.regime {
            symbol_state.regime = target_regime;
            symbol_state.regime_entered_at = Some(now);
        } else if symbol_state.regime_entered_at.is_none() {
            symbol_state.regime_entered_at = Some(now);
        }
        let regime = symbol_state.regime;

        // Continuous intensity: flow magnitude drives toxic intensity, vol drives volatile intensity.
        let flow_intensity = flow_abs.min(Decimal::ONE);
        let vol_intensity = (volatility / Decimal::new(8, 4)).min(Decimal::ONE);
        let raw_intensity = flow_intensity.max(vol_intensity);
        // EWMA-smooth the intensity to prevent jitter.
        symbol_state.regime_intensity = Decimal::new(2, 1) * raw_intensity
            + Decimal::new(8, 1) * symbol_state.regime_intensity;
        let regime_intensity = symbol_state.regime_intensity.min(Decimal::ONE).max(Decimal::ZERO);

        let time_multiplier = time_of_day_multiplier(now);
        let price_index = match pair.price_source {
            PriceSource::Mark => market.mark_price.or_else(|| market.mid_price())?,
            PriceSource::Spot => market.spot_price.or_else(|| market.mid_price())?,
            PriceSource::Mid => mid,
        };
        let base_volatility_addon = factor_spread_addon(parsed, volatility);
        let volatility_addon =
            (base_volatility_addon + bbo_spread_component + cross_flow.abs()) * time_multiplier;

        if symbol_state.bbo_spread_ewma.is_none() {
            return None;
        }

        // S6: volume_imbalance is now purely volume-based size scaling (always >= 0).
        // flow_direction carries the signed directional pressure separately.
        let combined_flow_direction = symbol_state.smoothed_flow + cross_flow;
        let volume_size_addon = rolling_volume * parsed.factors.volume_size_weight;
        let volume_imbalance = volume_size_addon.max(Decimal::ZERO);

        Some(FactorSnapshot {
            price_index,
            raw_volatility: volatility,
            volatility: volatility_addon,
            volume_imbalance,
            flow_direction: combined_flow_direction.clamp(-Decimal::ONE, Decimal::ONE),
            inventory_skew,
            recent_trade_count,
            regime,
            regime_intensity,
        })
    }
}

pub fn factor_spread_addon(parsed: &ParsedConfig, volatility: Decimal) -> Decimal {
    volatility * parsed.factors.volatility_spread_weight
}

fn cross_symbol_flow(
    symbol: &str,
    state: &BotState,
    window_start: DateTime<Utc>,
    weight: Decimal,
) -> Decimal {
    let mut total = Decimal::ZERO;
    let mut count = 0u64;
    for (other_symbol, market) in &state.market.symbols {
        if other_symbol == symbol {
            continue;
        }
        let mut buy = Decimal::ZERO;
        let mut sell = Decimal::ZERO;
        for trade in market
            .recent_trades
            .iter()
            .filter(|trade| trade.timestamp >= window_start)
        {
            match trade.taker_side {
                Some(crate::domain::Side::Bid) => buy += trade.quantity,
                Some(crate::domain::Side::Ask) => sell += trade.quantity,
                None => {}
            }
        }
        let total_volume = buy + sell;
        if total_volume > Decimal::ZERO {
            total += (buy - sell) / total_volume;
            count += 1;
        }
    }

    if count == 0 {
        Decimal::ZERO
    } else {
        (total / Decimal::from(count)) * weight
    }
}

/// S5: Pure regime candidate from signals, without dwell-time hysteresis.
fn candidate_regime(
    flow_abs: Decimal,
    volatility: Decimal,
    trade_velocity_ratio: Decimal,
    trade_velocity: Decimal,
) -> MarketRegime {
    // Directional flow threshold: net imbalance > 60% of volume → toxic trend.
    if flow_abs > Decimal::new(6, 1) {
        return MarketRegime::TrendingToxic;
    }
    // Volatile: high vol, high velocity ratio vs baseline, OR raw velocity > 1.5 trades/sec
    // (= 90 trades/min). The raw velocity check catches balanced-flow high-volume activity
    // that flow_abs misses when buy and sell are equal.
    if volatility > Decimal::new(8, 4)
        || trade_velocity_ratio > Decimal::new(15, 1)
        || trade_velocity > Decimal::new(15, 1)
    {
        return MarketRegime::VolatileBalanced;
    }
    MarketRegime::Quiet
}

/// S8: GRVT/Asia-aware time-of-day spread multiplier.
/// Most liquid 0-8 UTC (Tokyo + early London) → tighter spreads.
/// Least liquid 12-20 UTC (US morning but Asian night) → wider spreads.
fn time_of_day_multiplier(now: DateTime<Utc>) -> Decimal {
    let hour = now.hour();

    let base = if (0..8).contains(&hour) {
        // Asian session: highest GRVT liquidity → narrow spreads.
        Decimal::new(8, 1)
    } else if (8..12).contains(&hour) {
        // London open / Asia close transition.
        Decimal::ONE
    } else if (12..20).contains(&hour) {
        // US morning + Asian night: least liquid for GRVT.
        Decimal::new(13, 1)
    } else {
        // Late US / early Asia pre-market.
        Decimal::new(11, 1)
    };

    // Widen during funding rate windows (every 8 h, at 0, 8, 16 UTC).
    let funding_hour = matches!(hour, 0 | 8 | 16) && now.minute() >= 45;
    let funding_next = matches!(hour, 7 | 15 | 23) && now.minute() >= 45;
    if funding_hour || funding_next {
        base * Decimal::new(15, 1)
    } else {
        base
    }
}
