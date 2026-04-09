# Market Making Bot

Rust market making skeleton for Hibachi with exchange abstractions so we can add more venues later.

## Current shape

- Typed TOML config for venue credentials, runtime, risk, factors, pair-level quoting, and Telegram alerts.
- Configurable pacing and retry policy for REST calls plus websocket auto-reconnect.
- Parameterized linear grid with inventory skew, Gaussian sizing, volatility controls, and differential reconciliation.
- Exchange-agnostic traits for market data, private data, order execution, and instrument discovery.
- Hibachi adapter for:
  - `GET /market/exchange-info`
  - `GET /trade/orders`
  - `POST /trade/orders`
  - `DELETE /trade/orders`
  - public market websocket subscriptions
  - private account websocket connection
- Bot runtime with market aggregation, private orderbook state, fill and position tracking, PnL refresh, factor engine, quote distribution generation, reconciler, circuit breaker, startup/shutdown cancel-all, and Telegram alerting.

## Run

```bash
cargo run -- config/settings.toml
```

Set `runtime.dry_run = true` to stream data and compute quotes without sending order or cancel requests.

## Docker

Build and run with Docker Compose:

```bash
docker compose up --build
```

The compose file mounts `config/settings.toml` into the container, so keep your real local secrets there and use `config/settings.example.toml` as the committed template.

Tune request pacing and retry behavior in the `[network]` section:
- `private_rest_min_interval_ms`: minimum delay between REST calls
- `retry_max_attempts`: total attempts for retryable REST failures
- `retry_initial_backoff_ms` / `retry_max_backoff_ms`: exponential backoff window
- `websocket_reconnect_backoff_ms`: reconnect delay after websocket disconnects

The `[model]` section controls the quote model:
- `born_inf_bps` / `born_sup_bps`: inner and outer linear spread bounds
- `v0`, `mu`, `sigma`, `n_points`: Gaussian ladder sizing and number of quote levels
- `position_spread_multiplier` / `position_dead_zone`: inventory skew control
- `price_sensitivity_threshold`, `price_sensitivity_scaling_factor`, `volume_sensitivity_threshold`: diff tolerance tuning
- `volatility_cut_threshold`, `index_forward_buffer_bps`, `prevent_spread_crossing`, `cancel_orders_crossing_mid`: safety rules

## Important notes

- `wss://api.hibachi.xyz/ws/account` is wired as the private websocket endpoint based on the official SDK patterns; if Hibachi changes the path or message schema, update `config/settings.toml` and `src/exchange/hibachi.rs`.
- Hibachi's public docs did not clearly publish concrete numeric rate-limit thresholds when this scaffold was built, so the retry/rate-limit policy is conservative and fully configurable rather than hardcoded to undocumented limits.
- The private websocket parser is intentionally conservative right now because Hibachi's public docs expose the market websocket schema much more clearly than the account stream schema.
- The reconciler currently uses a safe but simple strategy: if desired quotes do not match tracked exchange orders, it cancels all and reposts the target ladder.
- Before live trading, add a dry-run mode, deeper private-stream decoding, order amend support, and exchange-specific rate limiting.
