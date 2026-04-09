use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstrumentMeta {
    pub symbol: String,
    pub contract_id: u32,
    pub underlying_decimals: u32,
    pub settlement_decimals: u32,
    pub min_order_size: Decimal,
    pub tick_size: Option<Decimal>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Ask,
    Bid,
}

impl Side {
    pub fn sign(self) -> Decimal {
        match self {
            Side::Bid => Decimal::ONE,
            Side::Ask => -Decimal::ONE,
        }
    }

    pub fn opposite(self) -> Self {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

impl Default for Side {
    fn default() -> Self {
        Self::Bid
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarketRegime {
    #[default]
    Quiet,
    VolatileBalanced,
    TrendingToxic,
}

#[derive(Clone, Debug)]
pub struct QuoteIntent {
    pub symbol: String,
    pub level_index: usize,
    pub side: Side,
    pub price: Decimal,
    pub quantity: Decimal,
    pub post_only: bool,
}

#[derive(Clone, Debug)]
pub struct OrderRequest {
    pub symbol: String,
    pub contract_id: u32,
    pub level_index: usize,
    pub side: Side,
    pub order_type: OrderType,
    pub price: Option<Decimal>,
    pub quantity: Decimal,
    pub post_only: bool,
}

#[derive(Clone, Debug, Default)]
pub struct OpenOrder {
    pub order_id: Option<String>,
    pub nonce: u64,
    pub level_index: Option<usize>,
    pub symbol: String,
    pub side: Side,
    pub price: Option<Decimal>,
    pub remaining_quantity: Decimal,
}

#[derive(Clone, Debug)]
pub struct Fill {
    pub symbol: String,
    pub side: Side,
    pub price: Decimal,
    pub quantity: Decimal,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug, Default)]
pub struct Position {
    pub symbol: String,
    pub quantity: Decimal,
    pub entry_price: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
}

#[derive(Clone, Debug)]
pub enum MarketEvent {
    StreamReconnected {
        symbols: Vec<String>,
    },
    MarkPrice {
        symbol: String,
        price: Decimal,
        timestamp: DateTime<Utc>,
    },
    SpotPrice {
        symbol: String,
        price: Decimal,
        timestamp: DateTime<Utc>,
    },
    BestBidAsk {
        symbol: String,
        bid: Decimal,
        ask: Decimal,
        timestamp: DateTime<Utc>,
    },
    Trade {
        symbol: String,
        price: Decimal,
        quantity: Decimal,
        taker_side: Option<Side>,
        timestamp: DateTime<Utc>,
    },
    OrderBookSnapshot {
        symbol: String,
        bids: BTreeMap<Decimal, Decimal>,
        asks: BTreeMap<Decimal, Decimal>,
        timestamp: DateTime<Utc>,
    },
    OrderBookUpdate {
        symbol: String,
        bids: BTreeMap<Decimal, Decimal>,
        asks: BTreeMap<Decimal, Decimal>,
        timestamp: DateTime<Utc>,
    },
}

#[derive(Clone, Debug)]
pub enum PrivateEvent {
    StreamReconnected,
    OpenOrders(Vec<OpenOrder>),
    UpsertOpenOrder(OpenOrder),
    RemoveOpenOrder {
        order_id: Option<String>,
        nonce: Option<u64>,
    },
    Fill(Fill),
    Position(Position),
    AccountEquity {
        equity: Decimal,
    },
}

#[derive(Clone, Debug, Default)]
pub struct FactorSnapshot {
    pub price_index: Decimal,
    pub raw_volatility: Decimal,
    pub volatility: Decimal,
    /// Pure volume-based size scalar, always >= 0. Use for order sizing. (S6)
    pub volume_imbalance: Decimal,
    /// Signed flow direction [-1, 1]: positive = buy pressure, negative = sell pressure. (S6)
    pub flow_direction: Decimal,
    pub inventory_skew: Decimal,
    pub recent_trade_count: usize,
    pub regime: MarketRegime,
    /// Continuous regime intensity in [0, 1]: 0 = fully Quiet, 1 = fully TrendingToxic. (S5)
    pub regime_intensity: Decimal,
}

#[derive(Clone, Debug)]
pub struct RecentTrade {
    pub quantity: Decimal,
    pub timestamp: DateTime<Utc>,
    pub taker_side: Option<Side>,
}
