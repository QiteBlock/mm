use std::{collections::HashMap, str::FromStr, time::Duration};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{handshake::client::generate_key, http::Request, Message as WsMessage},
    Connector,
};
use tracing::{info, warn};

use crate::domain::MarketEvent;

const BINANCE_FUTURES_WS_BASE: &str = "wss://fstream.binance.com/stream";
const BINANCE_FUTURES_REST_BASE: &str = "https://fapi.binance.com/fapi/v1/premiumIndex";
const RECONNECT_DELAY_SECS: u64 = 2;

/// Stream Binance perpetual futures mark prices and re-emit as `MarketEvent::SpotPrice`.
///
/// Two parallel data paths feed the same channel:
/// 1. WebSocket `markPrice@1s` with an explicit native-tls connector — fires every second.
/// 2. REST `premiumIndex` poll every 1 s — backup during WS reconnects.
///
/// GRVT streams use `connect_async` which defaults to rustls; Binance uses
/// `connect_async_tls_with_config` with `Connector::NativeTls` so the two
/// TLS stacks are selected independently.
pub async fn stream_binance_spot_prices(
    symbol_map: HashMap<String, String>,
    sender: mpsc::Sender<MarketEvent>,
) -> Result<()> {
    if symbol_map.is_empty() {
        std::future::pending::<()>().await;
        return Ok(());
    }

    // Spawn REST fallback — fires every 1 s independently of WS state.
    {
        let rest_map = symbol_map.clone();
        let rest_sender = sender.clone();
        tokio::spawn(async move {
            poll_binance_rest(rest_map, rest_sender).await;
        });
    }

    let streams: Vec<String> = symbol_map
        .values()
        .map(|s| format!("{}@markPrice@1s", s.to_lowercase()))
        .collect();
    let ws_url = format!("{}?streams={}", BINANCE_FUTURES_WS_BASE, streams.join("/"));

    // Reverse map: binance_symbol_lowercase -> grvt symbol.
    let reverse_map: HashMap<String, String> = symbol_map
        .iter()
        .map(|(grvt, binance)| (binance.to_lowercase(), grvt.clone()))
        .collect();

    // Build native-tls connector once; clone it each reconnect attempt.
    let tls_connector = native_tls::TlsConnector::builder()
        .build()
        .expect("native-tls connector build failed");

    loop {
        info!(%ws_url, "connecting to Binance futures markPrice@1s stream (native-tls)");

        let request = match build_ws_request(&ws_url) {
            Ok(r) => r,
            Err(e) => {
                warn!(err = %e, "failed to build Binance WS request; retrying");
                tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
                continue;
            }
        };

        let connector = Connector::NativeTls(tls_connector.clone());
        match connect_async_tls_with_config(request, None, false, Some(connector)).await {
            Ok((ws_stream, _)) => {
                let (mut write, mut read) = ws_stream.split();
                loop {
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

                    if let Some(payload) = pong_payload {
                        if let Err(e) = write.send(WsMessage::Pong(payload.into())).await {
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

/// Poll the Binance REST `premiumIndex` endpoint every 1 s per symbol.
/// Completely independent `reqwest::Client` — separate from the GRVT exchange client.
async fn poll_binance_rest(symbol_map: HashMap<String, String>, sender: mpsc::Sender<MarketEvent>) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(err = %e, "failed to build Binance REST client; REST fallback disabled");
            return;
        }
    };

    let mut ticker = {
        let mut t = tokio::time::interval(Duration::from_secs(1));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        t
    };

    loop {
        ticker.tick().await;
        let now = chrono::Utc::now();
        for (grvt_symbol, binance_symbol) in &symbol_map {
            let url = format!(
                "{}?symbol={}",
                BINANCE_FUTURES_REST_BASE,
                binance_symbol.to_uppercase()
            );
            match client.get(&url).send().await {
                Ok(resp) => match resp.json::<BinancePremiumIndex>().await {
                    Ok(index) => {
                        if let Ok(price) = Decimal::from_str(&index.mark_price) {
                            if price > Decimal::ZERO {
                                let _ = sender.try_send(MarketEvent::SpotPrice {
                                    symbol: grvt_symbol.clone(),
                                    price,
                                    timestamp: now,
                                });
                            }
                        }
                    }
                    Err(e) => warn!(err = %e, symbol = %binance_symbol, "Binance REST parse failed"),
                },
                Err(e) => warn!(err = %e, symbol = %binance_symbol, "Binance REST request failed"),
            }
        }
    }
}

fn build_ws_request(url: &str) -> Result<Request<()>> {
    let uri: tokio_tungstenite::tungstenite::http::Uri = url.parse()?;
    let host = uri.host().unwrap_or("fstream.binance.com");
    let request = Request::builder()
        .uri(url)
        .header("Host", host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .body(())?;
    Ok(request)
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

fn parse_mark_price(text: &str, reverse_map: &HashMap<String, String>) -> Option<(String, Decimal)> {
    let envelope: BinanceEnvelope = serde_json::from_str(text).ok()?;
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
    #[serde(rename = "p")]
    mark_price: String,
}

#[derive(Deserialize)]
struct BinancePremiumIndex {
    #[serde(rename = "markPrice")]
    mark_price: String,
}
