"""
GRVT Market Data Fetcher for HYPE/USDT-P
Fetches orderbook, trades, and mini-ticker data, then recommends config settings.
"""

import asyncio
import json
import math
import statistics
import time
from collections import deque
from dataclasses import dataclass, field
from typing import Optional

import aiohttp
import websockets

MARKET_DATA_BASE = "https://market-data.grvt.io"
MARKET_WS_URL = "wss://market-data.grvt.io/ws/full"
SYMBOL = "HYPE_USDT_Perp"
COLLECT_SECS = 30  # how long to collect live data


@dataclass
class MarketSnapshot:
    # Instrument meta
    tick_size: float = 0.0
    min_size: float = 0.0
    min_notional: float = 0.0

    # BBO / ticker (maintained as running state from delta updates)
    mark_price: float = 0.0
    index_price: float = 0.0
    mid_price: float = 0.0
    best_bid_price: float = 0.0
    best_bid_size: float = 0.0
    best_ask_price: float = 0.0
    best_ask_size: float = 0.0

    # Trades (snapshot of last N)
    trade_prices: deque = field(default_factory=lambda: deque(maxlen=500))
    trade_sizes: deque = field(default_factory=lambda: deque(maxlen=500))
    trade_timestamps_ns: deque = field(default_factory=lambda: deque(maxlen=500))
    buy_count: int = 0
    sell_count: int = 0

    # Orderbook (price -> size dict, maintained incrementally)
    bids: dict = field(default_factory=dict)
    asks: dict = field(default_factory=dict)

    def mid(self) -> float:
        if self.mid_price:
            return self.mid_price
        if self.best_bid_price and self.best_ask_price:
            return (self.best_bid_price + self.best_ask_price) / 2
        return self.mark_price

    def bbo_spread_bps(self) -> Optional[float]:
        mid = self.mid()
        if mid and self.best_bid_price and self.best_ask_price:
            return (self.best_ask_price - self.best_bid_price) / mid * 10000
        return None

    def realised_vol_annualised(self) -> Optional[float]:
        """Log-return vol from recent trades, annualised."""
        prices = list(self.trade_prices)
        if len(prices) < 10:
            return None
        log_returns = [
            math.log(prices[i] / prices[i - 1])
            for i in range(1, len(prices))
            if prices[i - 1] > 0
        ]
        if len(log_returns) < 5:
            return None
        std = statistics.stdev(log_returns)
        # Estimate trades-per-second from timestamps
        ts = list(self.trade_timestamps_ns)
        span_ns = ts[-1] - ts[0] if len(ts) > 1 else 0
        trades_per_sec = len(ts) / (span_ns / 1e9) if span_ns > 0 else 1.0
        annualised = std * math.sqrt(trades_per_sec * 365 * 24 * 3600)
        return annualised

    def avg_trade_size_usd(self) -> Optional[float]:
        mid = self.mid()
        if not mid or not self.trade_sizes:
            return None
        return statistics.mean(self.trade_sizes) * mid

    def trade_rate_per_sec(self) -> float:
        ts = list(self.trade_timestamps_ns)
        if len(ts) < 2:
            return 0.0
        span_ns = ts[-1] - ts[0]
        if span_ns <= 0:
            return 0.0
        return len(ts) / (span_ns / 1e9)

    def flow_imbalance(self) -> Optional[float]:
        total = self.buy_count + self.sell_count
        if total == 0:
            return None
        return (self.buy_count - self.sell_count) / total

    def orderbook_depth_usd(self, side: str, bps: float = 50) -> float:
        """Sum USD depth within `bps` basis points of mid."""
        mid = self.mid()
        if not mid:
            return 0.0
        book = self.bids if side == "bid" else self.asks
        total = 0.0
        for price_str, size in book.items():
            price = float(price_str)
            if side == "bid" and price >= mid * (1 - bps / 10000):
                total += price * size
            elif side == "ask" and price <= mid * (1 + bps / 10000):
                total += price * size
        return total


async def fetch_instrument_info(session: aiohttp.ClientSession, snap: MarketSnapshot):
    url = f"{MARKET_DATA_BASE}/full/v1/all_instruments"
    async with session.post(url, json={"kind": ["PERPETUAL"]}) as resp:
        data = await resp.json()
    instruments = data.get("result", [])
    for inst in instruments:
        if inst.get("instrument") == SYMBOL:
            snap.tick_size = float(inst.get("tick_size", 0))
            snap.min_size = float(inst.get("min_size", 0))
            snap.min_notional = float(inst.get("min_notional", 0))
            print(f"\n=== Instrument: {SYMBOL} ===")
            print(f"  tick_size      : {snap.tick_size}")
            print(f"  min_size       : {snap.min_size}")
            print(f"  min_notional   : {snap.min_notional}")
            print(f"  base_decimals  : {inst.get('base_decimals')}")
            print(f"  full data      : {json.dumps(inst, indent=2)}")
            return
    print(f"  [warn] {SYMBOL} not found in instruments")


def apply_mini_update(snap: MarketSnapshot, feed: dict):
    """Apply delta mini-ticker update onto running state."""
    for field_name in ("mark_price", "index_price", "mid_price",
                       "best_bid_price", "best_bid_size",
                       "best_ask_price", "best_ask_size"):
        if field_name in feed:
            setattr(snap, field_name, float(feed[field_name]))


def apply_book_update(snap: MarketSnapshot, feed: dict):
    """Apply delta orderbook update. size=0 means remove level."""
    for entry in feed.get("bids", []):
        price, size = entry["price"], float(entry["size"])
        if size == 0:
            snap.bids.pop(price, None)
        else:
            snap.bids[price] = size
    for entry in feed.get("asks", []):
        price, size = entry["price"], float(entry["size"])
        if size == 0:
            snap.asks.pop(price, None)
        else:
            snap.asks[price] = size


async def collect_ws_data(snap: MarketSnapshot):
    deadline = time.monotonic() + COLLECT_SECS
    print(f"\nConnecting to WebSocket: {MARKET_WS_URL}")

    async with websockets.connect(MARKET_WS_URL, ping_interval=20) as ws:
        for req_id, (stream, selector) in enumerate([
            ("v1.mini.d",  f"{SYMBOL}@100"),
            ("v1.trade",   f"{SYMBOL}@50"),
            ("v1.book.d",  f"{SYMBOL}@500"),
        ], start=1):
            await ws.send(json.dumps({
                "jsonrpc": "2.0", "method": "subscribe",
                "params": {"stream": stream, "selectors": [selector]},
                "id": req_id,
            }))

        print(f"Subscribed — collecting {COLLECT_SECS}s of live data…")

        while time.monotonic() < deadline:
            try:
                raw = await asyncio.wait_for(ws.recv(), timeout=2.0)
            except asyncio.TimeoutError:
                continue

            msg = json.loads(raw)
            stream = msg.get("stream", "")
            feed = msg.get("feed", {})

            if not stream or not feed:
                # subscription ack or error
                if "error" in msg:
                    print(f"  [ws error] {msg['error']}")
                continue

            if "mini" in stream:
                apply_mini_update(snap, feed)

            elif "trade" in stream:
                price = float(feed.get("price", 0) or 0)
                size = float(feed.get("size", 0) or 0)
                ts = int(feed.get("event_time", 0) or 0)
                if price > 0:
                    snap.trade_prices.append(price)
                    snap.trade_sizes.append(size)
                    snap.trade_timestamps_ns.append(ts)
                    if feed.get("is_taker_buyer"):
                        snap.buy_count += 1
                    else:
                        snap.sell_count += 1

            elif "book" in stream:
                apply_book_update(snap, feed)

    print(f"Collection done. Trades: {len(snap.trade_prices)}, "
          f"Book levels bid/ask: {len(snap.bids)}/{len(snap.asks)}")


def recommend_config(snap: MarketSnapshot):
    mid = snap.mid()
    if not mid:
        print("\n[!] No mid price — cannot recommend config.")
        return

    vol = snap.realised_vol_annualised()
    bbo_spread = snap.bbo_spread_bps()
    avg_trade_usd = snap.avg_trade_size_usd()
    trade_rate = snap.trade_rate_per_sec()
    bid_depth_50 = snap.orderbook_depth_usd("bid", 50)
    ask_depth_50 = snap.orderbook_depth_usd("ask", 50)
    flow_imb = snap.flow_imbalance()
    daily_vol = (vol / math.sqrt(365)) if vol else None

    print("\n" + "=" * 60)
    print("MARKET SUMMARY")
    print("=" * 60)
    print(f"  Mid price          : {mid:.4f}")
    print(f"  Mark price         : {snap.mark_price:.4f}")
    print(f"  Index price        : {snap.index_price:.4f}")
    print(f"  Best bid           : {snap.best_bid_price:.4f}  ({snap.best_bid_size:.2f} HYPE)")
    print(f"  Best ask           : {snap.best_ask_price:.4f}  ({snap.best_ask_size:.2f} HYPE)")
    print(f"  BBO spread         : {bbo_spread:.3f} bps" if bbo_spread else "  BBO spread         : n/a")
    print(f"  Tick size          : {snap.tick_size}")
    print(f"  Min size           : {snap.min_size}  (≈ ${snap.min_size * mid:.2f})")
    print(f"  Min notional       : ${snap.min_notional}")
    print(f"  Realised vol (ann) : {vol:.1%}" if vol else "  Realised vol       : n/a (too few trades)")
    print(f"  Daily vol          : {daily_vol:.2%}" if daily_vol else "")
    print(f"  Avg trade size USD : ${avg_trade_usd:,.0f}" if avg_trade_usd else "  Avg trade size USD : n/a")
    print(f"  Trade rate /sec    : {trade_rate:.2f}")
    print(f"  Flow imbalance     : {flow_imb:+.2%}  (buy {snap.buy_count} / sell {snap.sell_count})" if flow_imb is not None else "")
    print(f"  Bid depth 50bps    : ${bid_depth_50:,.0f}")
    print(f"  Ask depth 50bps    : ${ask_depth_50:,.0f}")

    # -------------------------
    # Derived recommendations
    # -------------------------

    # Spread floors
    tick_bps = snap.tick_size / mid * 10000 if snap.tick_size and mid else 0.03
    natural_spread = bbo_spread if bbo_spread else tick_bps * 2

    born_inf = max(tick_bps, round(natural_spread * 0.5, 1))
    born_sup = max(born_inf + tick_bps, round(natural_spread * 1.2, 1))
    max_spread = max(20, round(natural_spread * 8, 0))

    # Vol-based
    sigma = round(vol, 2) if vol else 1.20
    vol_cut = round(daily_vol * 0.3, 5) if daily_vol else 0.010
    # volatility_spread_weight: calibrate so at 1× daily vol, extra spread ≈ born_inf
    # spread_add = vol_spread_weight × realized_vol_per_tick → empirically 200–400 for mid-vol assets
    vsw = round(born_inf / (daily_vol * 100), 0) if daily_vol and daily_vol > 0 else 250
    vsw = max(50, min(500, vsw))

    # Trade-size weight
    vol_size_w = round(1.0 / max(avg_trade_usd, 10), 4) if avg_trade_usd else 0.010

    # Burst threshold: avg + 2σ of trade rate is a good burst
    burst = round(max(1.3, trade_rate * 2.5), 1) if trade_rate > 0.2 else 1.5

    # Min trade: max(min_notional, 2 × min_size_usd), floor at 5
    min_trade = max(snap.min_notional, snap.min_size * mid * 2, 5.0)
    min_trade = round(min_trade, 1)

    # Emergency widening: ~4× born_sup
    emerg_widen = max(10, int(born_sup * 4))

    # inventory_skew_weight: higher for thinner markets
    depth_ratio = (bid_depth_50 + ask_depth_50) / 2 if (bid_depth_50 + ask_depth_50) > 0 else 50000
    inv_skew = round(max(0.5, min(3.0, 50000 / max(depth_ratio, 1000))), 2)

    print("\n" + "=" * 60)
    print("RECOMMENDATIONS")
    print("=" * 60)

    print(f"""
[model]
born_inf_bps = "{born_inf:.1f}"           # half the natural BBO spread ({natural_spread:.2f} bps observed)
born_sup_bps = "{born_sup:.1f}"           # 1.2x natural spread
sigma = "{sigma:.2f}"                     # annualised realised vol {"("+f"{vol:.1%} observed)" if vol else "(default - no vol data)"}
min_step_price = "{snap.tick_size}"       # = tick size
min_step_volume = "{snap.min_size}"       # = min lot size
min_trade_amount = "{min_trade}"          # max(min_notional, 2x min lot)
max_spread_bps = "{int(max_spread)}"      # ~8x natural spread (circuit-breaker level)
volatility_cut_threshold = "{vol_cut:.5f}"  # 30% of daily vol ({f"{daily_vol:.2%}" if daily_vol else "n/a"} daily)

[factors]
volatility_spread_weight = "{int(vsw)}"   # scales vol -> spread; calibrated to natural spread
volume_size_weight = "{vol_size_w:.4f}"   # per-USD of avg trade (avg ~${avg_trade_usd:,.0f})
trade_velocity_burst_threshold = "{burst:.1f}"  # ~2.5x observed trade rate ({trade_rate:.2f}/s)
inventory_skew_weight = "{inv_skew:.2f}"  # lighter book -> more skew; depth 50bps ~${(bid_depth_50+ask_depth_50)/2:,.0f}

[risk]
min_trade_amount = "{min_trade}"          # matches min_notional
emergency_widening_bps = "{emerg_widen}" # 4x born_sup
""")

    print("=" * 60)
    print("NOTES")
    print("=" * 60)
    print(f"  - HYPE_USDT_Perp has a VERY tight spread ({natural_spread:.3f} bps).")
    print(f"    born_inf must be >= tick_size ({tick_bps:.3f} bps) to avoid crossing.")
    if vol and vol > 2.0:
        print(f"  - Annualised vol {vol:.0%} is HIGH -- widen spreads and lower position limits.")
    if avg_trade_usd and avg_trade_usd > 5000:
        print(f"  - Large avg trade (${avg_trade_usd:,.0f}) -- set volume_size_weight low to avoid over-reaction.")
    if flow_imb is not None and abs(flow_imb) > 0.3:
        print(f"  - Strong flow imbalance ({flow_imb:+.0%}) -- consider increasing inventory_skew_weight.")


async def main():
    snap = MarketSnapshot()

    async with aiohttp.ClientSession() as session:
        print("Fetching instrument info…")
        await fetch_instrument_info(session, snap)

    await collect_ws_data(snap)
    recommend_config(snap)


if __name__ == "__main__":
    asyncio.run(main())
