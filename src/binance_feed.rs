use std::{collections::HashMap, str::FromStr, time::Duration};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
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
/// `bookTicker` fires on every BBO change (sub-millisecond, event-driven).
/// Mid-price is computed as (bid+ask)/2 locally.
///
/// Ping frames from Binance are answered with Pong to prevent the 3-minute
/// idle disconnect.  Events are sent with `try_send` so a full channel never
/// blocks the WS read loop.  The read loop drains all buffered frames and
/// keeps only the latest price per symbol (LIFO collapse) before forwarding.
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
    // Envelope stream field looks like "resolvusdt@bookTicker"; split at '@' → "resolvusdt".
    let reverse_map: HashMap<String, String> = symbol_map
        .iter()
        .map(|(grvt, binance)| (binance.to_lowercase(), grvt.clone()))
        .collect();

    loop {
        info!(%url, "connecting to Binance futures bookTicker stream");
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                let (mut write, mut read) = ws_stream.split();
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
                    let mut needs_reconnect = false;
                    let mut pong_payload: Option<Vec<u8>> = None;

                    process_frame(first, &reverse_map, &mut latest, &mut needs_reconnect, &mut pong_payload);

                    // Drain any additional buffered frames without blocking.
                    loop {
                        match tokio::time::timeout(Duration::ZERO, read.next()).await {
                            Ok(Some(Ok(frame))) => {
                                process_frame(frame, &reverse_map, &mut latest, &mut needs_reconnect, &mut pong_payload);
                            }
                            // Timeout (no more buffered) or stream end: drain complete.
                            _ => break,
                        }
                    }

                    // Respond to Ping. Binance sends a Ping every ~3 minutes;
                    // failure to respond causes a disconnect.
                    if let Some(payload) = pong_payload {
                        if let Err(e) = write.send(WsMessage::Pong(payload)).await {
                            warn!(err = %e, "failed to send Pong; reconnecting");
                            needs_reconnect = true;
                        }
                    }

                    if needs_reconnect {
                        break;
                    }

                    // Forward only the freshest price per symbol.
                    // try_send never blocks — drops if channel is full, which is fine because
                    // a newer event arrives momentarily and the engine deduplicates anyway.
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

fn process_frame(
    frame: WsMessage,
    reverse_map: &HashMap<String, String>,
    latest: &mut HashMap<String, Decimal>,
    needs_reconnect: &mut bool,
    pong_payload: &mut Option<Vec<u8>>,
) {
    match frame {
        WsMessage::Text(text) => {
            if let Some((sym, price)) = parse_book_ticker(&text, reverse_map) {
                latest.insert(sym, price);
            }
        }
        WsMessage::Ping(payload) => {
            // Always take the latest ping payload; one Pong reply is enough.
            *pong_payload = Some(payload.to_vec());
        }
        WsMessage::Close(_) => {
            warn!("Binance WS close frame received; reconnecting");
            *needs_reconnect = true;
        }
        _ => {}
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
    #[serde(rename = "b")]
    best_bid: String,
    #[serde(rename = "a")]
    best_ask: String,
}
