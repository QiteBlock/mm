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

/// Stream Binance perpetual futures mark prices (`markPrice@1s`) and re-emit as
/// `MarketEvent::SpotPrice` for the corresponding GRVT symbol.
///
/// `symbol_map` maps GRVT symbol names (e.g. `"RESOLV/USDT-P"`) to the
/// Binance futures symbol (e.g. `"resolvusdt"`).
///
/// `markPrice@1s` fires every second regardless of market activity — correct
/// for illiquid pairs where `bookTicker` may not fire at all.
///
/// Ping frames from Binance are answered with Pong to prevent the 3-minute
/// idle disconnect.  Events are sent with `try_send` so a full channel never
/// blocks the WS read loop.  The read loop drains all buffered frames and
/// keeps only the latest price per symbol before forwarding.
pub async fn stream_binance_spot_prices(
    symbol_map: HashMap<String, String>,
    sender: mpsc::Sender<MarketEvent>,
) -> Result<()> {
    if symbol_map.is_empty() {
        std::future::pending::<()>().await;
        return Ok(());
    }

    // markPrice@1s: fires every second unconditionally.
    let streams: Vec<String> = symbol_map
        .values()
        .map(|s| format!("{}@markPrice@1s", s.to_lowercase()))
        .collect();
    let url = format!("{}?streams={}", BINANCE_FUTURES_WS_BASE, streams.join("/"));

    // Reverse map: binance_symbol_lowercase -> grvt symbol.
    // Envelope stream field looks like "resolvusdt@markPrice"; split at '@' → "resolvusdt".
    let reverse_map: HashMap<String, String> = symbol_map
        .iter()
        .map(|(grvt, binance)| (binance.to_lowercase(), grvt.clone()))
        .collect();

    loop {
        info!(%url, "connecting to Binance futures markPrice@1s stream");
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

                    // Drain: collect all already-buffered frames, keep only
                    // the latest price per symbol.
                    let mut latest: HashMap<String, Decimal> = HashMap::new();
                    let mut needs_reconnect = false;
                    let mut pong_payload: Option<Vec<u8>> = None;

                    process_frame(first, &reverse_map, &mut latest, &mut needs_reconnect, &mut pong_payload);

                    loop {
                        match tokio::time::timeout(Duration::ZERO, read.next()).await {
                            Ok(Some(Ok(frame))) => {
                                process_frame(frame, &reverse_map, &mut latest, &mut needs_reconnect, &mut pong_payload);
                            }
                            _ => break,
                        }
                    }

                    // Respond to Ping — Binance disconnects after ~3 min without Pong.
                    if let Some(payload) = pong_payload {
                        if let Err(e) = write.send(WsMessage::Pong(payload)).await {
                            warn!(err = %e, "failed to send Pong; reconnecting");
                            needs_reconnect = true;
                        }
                    }

                    if needs_reconnect {
                        break;
                    }

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
            if let Some((sym, price)) = parse_mark_price(&text, reverse_map) {
                latest.insert(sym, price);
            }
        }
        WsMessage::Ping(payload) => {
            *pong_payload = Some(payload.to_vec());
        }
        WsMessage::Close(_) => {
            warn!("Binance WS close frame received; reconnecting");
            *needs_reconnect = true;
        }
        _ => {}
    }
}

/// Parse a Binance markPrice combined-stream envelope into (grvt_symbol, mark_price).
fn parse_mark_price(text: &str, reverse_map: &HashMap<String, String>) -> Option<(String, Decimal)> {
    let envelope: BinanceEnvelope = serde_json::from_str(text).ok()?;
    // "resolvusdt@markPrice" → "resolvusdt"
    let prefix = envelope.stream.split('@').next()?;
    let grvt_symbol = reverse_map.get(prefix)?.clone();
    let price = Decimal::from_str(&envelope.data.mark_price).ok()?;
    if price <= Decimal::ZERO {
        return None;
    }
    Some((grvt_symbol, price))
}

#[derive(Deserialize)]
struct BinanceEnvelope {
    stream: String,
    data: BinanceMarkPriceData,
}

#[derive(Deserialize)]
struct BinanceMarkPriceData {
    /// Mark price as a decimal string.
    #[serde(rename = "p")]
    mark_price: String,
}
