use std::time::Duration;

use async_trait::async_trait;
use anyhow::{bail, Result};
use rust_decimal::Decimal;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{
    domain::Position,
    exchange::{AnyExchangeClient, OrderExecutor, PrivateDataSource},
    telegram::TelegramNotifier,
};

#[async_trait]
pub trait CleanupExchange: OrderExecutor + PrivateDataSource + Sync {
    async fn submit_limit_close_orders(&self, positions: &[Position]) -> Result<()>;
    fn supports_limit_cleanup(&self) -> bool;
}

pub async fn flatten_account_state(
    exchange: &(impl CleanupExchange + Sync),
    notifier: &TelegramNotifier,
    phase: &str,
) -> Result<()> {
    info!(phase, "canceling all exchange orders");
    exchange.cancel_all_orders().await?;

    if exchange.supports_limit_cleanup() {
        close_positions_with_limit_orders(exchange, notifier, phase).await
    } else {
        info!(
            phase,
            "limit-based cleanup not implemented for this venue; leaving positions unchanged"
        );
        Ok(())
    }
}

async fn close_positions_with_limit_orders(
    exchange: &(impl CleanupExchange + Sync),
    notifier: &TelegramNotifier,
    phase: &str,
) -> Result<()> {
    // Each outer attempt re-prices the close order to the current passive best price.
    // Between re-pricings we poll every 2 s for up to 15 s to give the maker order
    // time to fill before we cancel and reprice.
    const REPRICE_ATTEMPTS: usize = 10;
    const POLL_INTERVAL_MS: u64 = 2_000;
    const POLLS_PER_ATTEMPT: usize = 8; // 8 × 2 s = 16 s wait per reprice round

    for attempt in 1..=REPRICE_ATTEMPTS {
        let positions = exchange.fetch_positions().await?;
        let open_positions = active_positions(&positions);
        if open_positions.is_empty() {
            info!(phase, attempts = attempt - 1, "no open positions to close");
            return Ok(());
        }

        info!(
            phase,
            attempt,
            count = open_positions.len(),
            "placing passive maker close order and waiting for fill"
        );

        exchange.submit_limit_close_orders(&open_positions).await?;

        // Poll until filled or timeout, then reprice.
        for poll in 1..=POLLS_PER_ATTEMPT {
            sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            let remaining = active_positions(&exchange.fetch_positions().await?);
            if remaining.is_empty() {
                info!(phase, attempt, poll, "position closed by maker fill");
                return Ok(());
            }
            info!(
                phase,
                attempt,
                poll,
                remaining = remaining.len(),
                "waiting for maker fill"
            );
        }

        // Timed out waiting — cancel and reprice at next attempt.
        info!(phase, attempt, "reprice: cancelling stale close order");
        exchange.cancel_all_orders().await?;
        sleep(Duration::from_millis(300)).await;
    }

    let residual_positions = active_positions(&exchange.fetch_positions().await?);
    if residual_positions.is_empty() {
        info!(phase, "limit-close cleanup completed after retries");
        return Ok(());
    }

    let summary = residual_positions
        .iter()
        .map(|position| format!("{} {}", position.symbol, position.quantity))
        .collect::<Vec<_>>()
        .join(", ");
    warn!(phase, residual = %summary, "limit-close cleanup exhausted retries");
    notifier
        .send(format!(
            "cleanup could not fully close positions with limit orders ({phase}): {summary}"
        ))
        .await;
    bail!("cleanup could not fully close positions with limit orders: {summary}")
}

fn active_positions(positions: &[Position]) -> Vec<Position> {
    positions
        .iter()
        .filter(|position| position.quantity != Decimal::ZERO)
        .cloned()
        .collect()
}

#[async_trait]
impl CleanupExchange for AnyExchangeClient {
    async fn submit_limit_close_orders(&self, positions: &[Position]) -> Result<()> {
        match self {
            AnyExchangeClient::Grvt(client) => client.submit_limit_close_orders(positions).await,
            AnyExchangeClient::Hibachi(_) => Ok(()),
        }
    }

    fn supports_limit_cleanup(&self) -> bool {
        matches!(self, AnyExchangeClient::Grvt(_))
    }
}
