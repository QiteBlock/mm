use std::{collections::HashMap, str::FromStr, time::Duration};

use anyhow::Result;
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{info, warn};

use crate::domain::MarketEvent;

const BINANCE_FUTURES_WS_BASE: &str = "wss://fstream.binance.com/stream";
const RECONNECT_DELAY_SECS: u64 = 2;

/// Stream Binance perpetual futures best-bid/ask (bookTicker) and re-emit as
/// `MarketEvent::SpotPrice` for the corresponding GRVT symbol.
///
/// `symbol_map` maps GRVT symbol names (e.g. `"RESOLV/USDT-P"`) to the
/// Binance futures symbol (e.g. `"resolvusdt"`).
///
/// `bookTicker` fires on every BBO change — sub-millisecond latency vs the
/// 1-second cadence of `markPrice@1s`.  Mid-price (bid+ask)/2 is computed
/// locally.  Because Binance perp prices lead GRVT prices we can requote
/// before informed traders hit stale GRVT resting orders.
///
/// The read loop drains all buffered WS frames each iteration and keeps only
/// the latest price per symbol before forwarding.  Events are sent with
/// `try_send` so a full channel never blocks the WS read loop.
///
/// If `symbol_map` is empty the function parks indefinitely without opening
/// any connection.
pub async fn stream_binance_spot_prices(
    symbol_map: HashMap<String, String>,
    sender: mpsc::Sender<MarketEvent>,
) -> Result<()> {
    if symbol_map.is_empty() {
        std::future::pending::<()>().await;
        return Ok(());
    }

    // Build the Binance combined-stream URL using bookTicker (real-time BBO).
    let streams: Vec<String> = symbol_map
        .values()
        .map(|s| format!("{}@bookTicker", s.to_lowercase()))
        .collect();
    let url = format!("{}?streams={}", BINANCE_FUTURES_WS_BASE, streams.join("/"));

    // Reverse map: binance_symbol_lowercase -> grvt symbol.
    // Envelope stream field looks like "resolvusdt@bookTicker"; split at '@' gives "resolvusdt".
    let reverse_map: HashMap<String, String> = symbol_map
        .iter()
        .map(|(grvt, binance)| (binance.to_lowercase(), grvt.clone()))
        .collect();

    loop {
        info!(%url, "connecting to Binance futures bookTicker stream");
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                let (_write, mut read) = ws_stream.split();
                loop {
                    // Block until at least one frame arrives.
                    let first = match read.next().await {
                        Some(Ok(msg)) => msg,
                        Some(Err(e)) => {
                            warn!(err = %e, "Binance WS error; reconnecting");
                            break;
                        }
                        None => {
                            warn!("Binance WS closed; reconnecting");
                            break;
                        }
                    };

                    // LIFO drain: collect the first frame plus any already-buffered frames.
                    // Keep only the latest mid-price per symbol to avoid replaying stale data.
                    let mut latest: HashMap<String, Decimal> = HashMap::new();

                    if let WsMessage::Text(text) = &first {
                        if let Some((sym, price)) = parse_book_ticker(text, &reverse_map) {
                            latest.insert(sym, price);
                        }
                    } else if matches!(first, WsMessage::Close(_)) {
                        warn!("Binance WS closed; reconnecting");
                        break;
                    }

                    // Drain any additional buffered frames without blocking.
                    loop {
                        match tokio::time::timeout(Duration::ZERO, read.next()).await {
                            Ok(Some(Ok(WsMessage::Text(text)))) => {
                                if let Some((sym, price)) = parse_book_ticker(&text, &reverse_map) {
                                    latest.insert(sym, price);
                                }
                            }
                            Ok(Some(Ok(WsMessage::Close(_)))) => {
                                warn!("Binance WS closed mid-drain; reconnecting");
                                // Signal outer loop to break by clearing `latest` and returning
                                // a sentinel — just break drain, outer loop will error next iter.
                                break;
                            }
                            Ok(Some(Err(e))) => {
                                warn!(err = %e, "Binance WS error mid-drain");
                                break;
                            }
                            // Timeout or stream yielded None or non-text: drain complete.
                            _ => break,
                        }
                    }

                    // Forward only the freshest price per symbol.
                    // try_send: never blocks — drops if channel is full, which is fine because
                    // a newer event will arrive momentarily and the engine discards stale ones anyway.
                    let now = chrono::Utc::now();
                    for (grvt_symbol, price) in latest {
                        let _ = sender.try_send(MarketEvent::SpotPrice {
                            symbol: grvt_symbol,
                            price,
                            timestamp: now,
                        });
                    }
                }
            }
            Err(e) => {
                warn!(err = %e, "Binance WS connect failed; retrying");
            }
        }
        tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
    }
}

/// Parse a Binance bookTicker combined-stream envelope into (grvt_symbol, mid_price).
fn parse_book_ticker(text: &str, reverse_map: &HashMap<String, String>) -> Option<(String, Decimal)> {
    let envelope: BinanceEnvelope = serde_json::from_str(text).ok()?;
    // "resolvusdt@bookTicker" → "resolvusdt"
    let prefix = envelope.stream.split('@').next()?;
    let grvt_symbol = reverse_map.get(prefix)?.clone();
    let bid = Decimal::from_str(&envelope.data.best_bid).ok()?;
    let ask = Decimal::from_str(&envelope.data.best_ask).ok()?;
    if bid <= Decimal::ZERO || ask <= Decimal::ZERO || ask < bid {
        return None;
    }
    let mid = (bid + ask) / Decimal::TWO;
    Some((grvt_symbol, mid))
}

#[derive(Deserialize)]
struct BinanceEnvelope {
    stream: String,
    data: BinanceBookTickerData,
}

#[derive(Deserialize)]
struct BinanceBookTickerData {
    /// Best bid price.
    #[serde(rename = "b")]
    best_bid: String,
    /// Best ask price.
    #[serde(rename = "a")]
    best_ask: String,
}
