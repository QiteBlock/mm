use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;

use crate::domain::Side;

const OBSERVATION_WINDOW_SECS: i64 = 15;
const DECAY_TO_NEUTRAL_SECS: i64 = 20 * 60;

fn neutral_score() -> Decimal {
    Decimal::new(5, 1)
}

#[derive(Default)]
pub struct FillTracker {
    pending: VecDeque<TrackedFill>,
    scores: HashMap<String, HashMap<(Side, usize), LevelToxicityScore>>,
}

impl FillTracker {
    pub fn record_fill(
        &mut self,
        symbol: String,
        side: Side,
        level_index: usize,
        fill_price: Decimal,
        spread_earned_bps: Decimal,
        timestamp: DateTime<Utc>,
    ) {
        self.pending.push_back(TrackedFill {
            symbol,
            side,
            level_index,
            fill_price,
            spread_earned_bps: spread_earned_bps.max(Decimal::new(1, 6)),
            timestamp,
        });
    }

    pub fn observe_price(
        &mut self,
        symbol: &str,
        reference_price: Decimal,
        timestamp: DateTime<Utc>,
    ) {
        if reference_price <= Decimal::ZERO {
            return;
        }

        let mut finished = Vec::new();
        let mut remaining = VecDeque::with_capacity(self.pending.len());
        while let Some(tracked) = self.pending.pop_front() {
            if tracked.symbol != symbol {
                remaining.push_back(tracked);
                continue;
            }

            if timestamp - tracked.timestamp >= Duration::seconds(OBSERVATION_WINDOW_SECS) {
                finished.push((tracked, reference_price, timestamp));
            } else {
                remaining.push_back(tracked);
            }
        }
        self.pending = remaining;

        for (tracked, observed_price, observed_at) in finished {
            let adverse_bps = adverse_bps(tracked.side, tracked.fill_price, observed_price);
            let toxicity_ratio = adverse_bps / tracked.spread_earned_bps.max(Decimal::new(1, 6));
            let score = self
                .scores
                .entry(tracked.symbol)
                .or_default()
                .entry((tracked.side, tracked.level_index))
                .or_default();
            score.apply(toxicity_ratio, observed_at);
        }
    }

    pub fn decay(&mut self, now: DateTime<Utc>) {
        for levels in self.scores.values_mut() {
            for score in levels.values_mut() {
                score.decay(now);
            }
        }
    }

    pub fn toxicity_score(&self, symbol: &str, side: Side, level_index: usize) -> Decimal {
        self.scores
            .get(symbol)
            .and_then(|levels| levels.get(&(side, level_index)))
            .map(|score| score.value)
            .unwrap_or_else(neutral_score)
    }

    pub fn level_volume_multiplier(&self, symbol: &str, side: Side, level_index: usize) -> Decimal {
        let score = self.toxicity_score(symbol, side, level_index);
        // Softer curve: neutral score (0.5) → 1.0×, score=1.5 → 0.5×, score=0.0 → 1.5×.
        // A single bad fill (score ~0.75) only reduces size to ~0.85×, not 0.5×.
        (Decimal::TWO - score).clamp(Decimal::new(5, 1), Decimal::new(15, 1))
    }
}

#[derive(Clone)]
struct TrackedFill {
    symbol: String,
    side: Side,
    level_index: usize,
    fill_price: Decimal,
    spread_earned_bps: Decimal,
    timestamp: DateTime<Utc>,
}

#[derive(Default)]
struct LevelToxicityScore {
    value: Decimal,
    last_updated: Option<DateTime<Utc>>,
}

impl LevelToxicityScore {
    fn apply(&mut self, toxicity_ratio: Decimal, now: DateTime<Utc>) {
        self.decay(now);
        let alpha = Decimal::new(25, 2);
        if self.last_updated.is_none() || self.value.is_zero() {
            self.value = toxicity_ratio;
        } else {
            self.value = alpha * toxicity_ratio + (Decimal::ONE - alpha) * self.value;
        }
        self.last_updated = Some(now);
    }

    fn decay(&mut self, now: DateTime<Utc>) {
        let Some(last_updated) = self.last_updated else {
            if self.value.is_zero() {
                self.value = neutral_score();
            }
            return;
        };
        let elapsed = (now - last_updated).num_seconds().max(0) as f64;
        let half_life = DECAY_TO_NEUTRAL_SECS as f64;
        let decay = if half_life <= 0.0 {
            0.0
        } else {
            0.5f64.powf(elapsed / half_life)
        };
        if decay.is_nan() || decay.is_infinite() {
            self.last_updated = Some(now);
            return;
        }
        let decay = Decimal::from_f64_retain(decay).unwrap_or(Decimal::ONE);
        let neutral = neutral_score();
        self.value = neutral + (self.value - neutral) * decay;
        self.last_updated = Some(now);
    }
}

fn adverse_bps(side: Side, fill_price: Decimal, observed_price: Decimal) -> Decimal {
    if fill_price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let raw = match side {
        Side::Bid => (fill_price - observed_price) / fill_price,
        Side::Ask => (observed_price - fill_price) / fill_price,
    };
    raw.max(Decimal::ZERO) * Decimal::from(10_000u64)
}
