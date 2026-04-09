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

/// Stream Binance perpetual futures mark prices and re-emit them as
/// `MarketEvent::SpotPrice` for the corresponding GRVT symbol.
///
/// `symbol_map` maps GRVT symbol names (e.g. `"HYPE/USDT-P"`) to the
/// lowercase Binance futures base name (e.g. `"hype"`).  The Binance
/// `<sym>usdt@markPrice@1s` stream publishes a mark-price update every
/// second with very low latency.  Because Binance perpetual prices lead
/// GRVT prices by several hundred milliseconds we can update our
/// reservation price and requote before informed traders sweep stale GRVT
/// resting orders.
///
/// If `symbol_map` is empty the function parks indefinitely without
/// opening any connection, so it is safe to always spawn.
pub async fn stream_binance_spot_prices(
    symbol_map: HashMap<String, String>,
    sender: mpsc::Sender<MarketEvent>,
) -> Result<()> {
    if symbol_map.is_empty() {
        std::future::pending::<()>().await;
        return Ok(());
    }

    // Build the Binance multi-stream URL.
    // Each stream name is "<binance_lowercase_symbol>usdt@markPrice@1s".
    let streams: Vec<String> = symbol_map
        .values()
        .map(|s| format!("{}@markPrice@1s", s.to_lowercase()))
        .collect();
    let url = format!("{}?streams={}", BINANCE_FUTURES_WS_BASE, streams.join("/"));

    // Reverse map: binance_stream_prefix (without "@...") -> grvt symbol.
    // The stream field in the envelope looks like "hypeusdt@markPrice".
    let reverse_map: HashMap<String, String> = symbol_map
        .iter()
        .map(|(grvt, binance)| (binance.to_lowercase(), grvt.clone()))
        .collect();

    loop {
        info!(%url, "connecting to Binance futures mark-price stream");
        match connect_async(&url).await {
            Ok((ws_stream, _)) => {
                let (_write, mut read) = ws_stream.split();
                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(WsMessage::Text(text)) => {
                            if let Some(event) = parse_envelope(&text, &reverse_map) {
                                let _ = sender.send(event).await;
                            }
                        }
                        Ok(WsMessage::Ping(_)) => {}
                        Ok(WsMessage::Close(_)) => {
                            warn!("Binance WS closed; reconnecting");
                            break;
                        }
                        Err(e) => {
                            warn!(err = %e, "Binance WS error; reconnecting");
                            break;
                        }
                        _ => {}
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

fn parse_envelope(text: &str, reverse_map: &HashMap<String, String>) -> Option<MarketEvent> {
    let envelope: BinanceEnvelope = serde_json::from_str(text).ok()?;
    // stream = "hypeusdt@markPrice" → prefix = "hypeusdt"
    let prefix = envelope.stream.split('@').next()?;
    let grvt_symbol = reverse_map.get(prefix)?;
    let price = Decimal::from_str(&envelope.data.mark_price).ok()?;
    if price <= Decimal::ZERO {
        return None;
    }
    Some(MarketEvent::SpotPrice {
        symbol: grvt_symbol.clone(),
        price,
        timestamp: chrono::Utc::now(),
    })
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
