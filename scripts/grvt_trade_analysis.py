"""
GRVT Order History + Market Data Analysis
Authenticates, fetches order history + live market, produces a verdict.
"""

import asyncio
import json
import statistics
from collections import Counter
from datetime import datetime, timezone

import aiohttp

AUTH_BASE      = "https://edge.grvt.io"
TRADING_BASE   = "https://trades.grvt.io"
MARKET_BASE    = "https://market-data.grvt.io"
API_KEY        = "3BtB6U3hhhZPf0vCfvMwh13FVOs"
SUB_ACCOUNT_ID = "1684894788771625"
SYMBOL         = "HYPE_USDT_Perp"


def ns_to_dt(ns):
    return datetime.fromtimestamp(int(ns) / 1e9, tz=timezone.utc)


async def login(session):
    async with session.post(
        f"{AUTH_BASE}/auth/api_key/login",
        json={"api_key": API_KEY},
        headers={"Cookie": "rm=true;"},
    ) as resp:
        raw = resp.headers.getall("Set-Cookie", [])
        cookie = raw[0].split(";")[0] if raw else None
        account_id = resp.headers.get("X-Grvt-Account-Id", SUB_ACCOUNT_ID)
    return {"cookie": cookie, "account_id": account_id}


async def post_trading(session, auth, path, payload):
    hdrs = {"Cookie": auth["cookie"], "X-Grvt-Account-Id": auth["account_id"]}
    async with session.post(f"{TRADING_BASE}{path}", json=payload, headers=hdrs) as resp:
        return (await resp.json(content_type=None)).get("result", [])


async def main():
    async with aiohttp.ClientSession() as session:
        auth = await login(session)

        orders_task = asyncio.create_task(
            post_trading(session, auth, "/full/v1/order_history",
                         {"sub_account_id": SUB_ACCOUNT_ID, "instrument": SYMBOL, "limit": 500})
        )
        pos_task = asyncio.create_task(
            post_trading(session, auth, "/full/v1/positions",
                         {"sub_account_id": SUB_ACCOUNT_ID, "kind": ["PERPETUAL"]})
        )
        ticker_task = asyncio.create_task(
            session.post(f"{MARKET_BASE}/full/v1/ticker", json={"instrument": SYMBOL})
        )

        orders   = await orders_task
        positions = await pos_task
        ticker_resp = await ticker_task
        ticker = (await ticker_resp.json(content_type=None)).get("result", {})

    # ---- market state ----
    bb   = float(ticker.get("best_bid_price", 0) or 0)
    ba   = float(ticker.get("best_ask_price", 0) or 0)
    mark = float(ticker.get("mark_price", 0) or 0)
    mid  = (bb + ba) / 2 if bb and ba else mark
    bbo  = (ba - bb) / mid * 10000 if mid else 0

    # ---- time range ----
    times = sorted(int(o["metadata"]["create_time"]) for o in orders if o.get("metadata"))
    span_mins = (times[-1] - times[0]) / 1e9 / 60 if len(times) > 1 else 0
    oldest = ns_to_dt(times[0])
    newest = ns_to_dt(times[-1])

    # ---- status breakdown ----
    statuses = Counter(
        o["state"]["status"] for o in orders
        if isinstance(o.get("state"), dict)
    )
    reasons = Counter(
        o["state"].get("reject_reason", "?")
        for o in orders
        if isinstance(o.get("state"), dict) and o["state"]["status"] == "CANCELLED"
    )

    # ---- extract fills ----
    buy_fills, sell_fills = [], []
    for o in orders:
        if not isinstance(o.get("state"), dict):
            continue
        if o["state"]["status"] not in ("FILLED", "PARTIALLY_FILLED"):
            continue
        st = o["state"]
        traded_list = st.get("traded_size", [])
        price_list  = st.get("avg_fill_price", [])
        for i, leg in enumerate(o.get("legs", [])):
            traded = float(traded_list[i]) if i < len(traded_list) else 0
            fprice = float(price_list[i])  if i < len(price_list)  else 0
            if traded > 0 and fprice > 0:
                entry = {
                    "price": fprice, "size": traded, "usd": fprice * traded,
                    "limit": float(leg.get("limit_price", 0) or 0),
                    "ts": ns_to_dt(int(o["metadata"]["create_time"])),
                }
                if leg.get("is_buying_asset"):
                    buy_fills.append(entry)
                else:
                    sell_fills.append(entry)

    total_fills = len(buy_fills) + len(sell_fills)
    buy_qty  = sum(f["size"] for f in buy_fills)
    sell_qty = sum(f["size"] for f in sell_fills)
    buy_usd  = sum(f["usd"]  for f in buy_fills)
    sell_usd = sum(f["usd"]  for f in sell_fills)
    avg_buy  = buy_usd  / buy_qty  if buy_qty  else 0
    avg_sell = sell_usd / sell_qty if sell_qty else 0

    # ---- quote placement stats ----
    bids_dist, asks_dist = [], []
    for o in orders:
        if not isinstance(o.get("state"), dict):
            continue
        if o["state"]["status"] != "CANCELLED":
            continue
        for leg in o.get("legs", []):
            px = float(leg.get("limit_price", 0) or 0)
            if px > 0 and mid:
                if leg.get("is_buying_asset"):
                    bids_dist.append((mid - px) / mid * 10000)
                else:
                    asks_dist.append((px - mid) / mid * 10000)

    # ---- print report ----
    sep = "=" * 62

    print(sep)
    print("GRVT MARKET MAKING ANALYSIS  --  HYPE_USDT_Perp")
    print(sep)
    print(f"Period : {oldest.strftime('%H:%M:%S')} -> {newest.strftime('%H:%M:%S')} UTC  ({span_mins:.1f} min)")
    print(f"Mid    : {mid:.4f}   Mark: {mark:.4f}   BBO: {bbo:.3f} bps  (1 tick = 0.28 bps)")

    print(f"\n--- ORDER FLOW  ({len(orders)} orders) ---")
    print(f"  Filled      : {statuses.get('FILLED', 0)}  ({statuses.get('FILLED', 0) / len(orders) * 100:.1f}%)")
    print(f"  Cancelled   : {statuses.get('CANCELLED', 0)}")
    print(f"    CLIENT_CANCEL      {reasons.get('CLIENT_CANCEL', 0):4d}  -- price moved, bot re-quoted")
    print(f"    CLIENT_BULK_CANCEL {reasons.get('CLIENT_BULK_CANCEL', 0):4d}  -- bot cancel-all cycle")
    print(f"    EXPIRED            {reasons.get('EXPIRED', 0):4d}  -- 15s TTL elapsed, price did not move enough")
    if span_mins > 0:
        rate = len(orders) / span_mins
        cycle_secs = 60 / rate * 4
        print(f"  Rate   : {rate:.1f} orders/min  (~{cycle_secs:.1f}s per reconcile cycle, 4 orders/cycle)")

    print(f"\n--- FILLS  ({total_fills} total) ---")
    if total_fills == 0:
        print("  No fills yet.")
    else:
        print(f"  Buys  : {len(buy_fills)}  qty={buy_qty:.4f} HYPE  avg_px={avg_buy:.4f}  total=${buy_usd:.2f}")
        print(f"  Sells : {len(sell_fills)}  qty={sell_qty:.4f} HYPE  avg_px={avg_sell:.4f}  total=${sell_usd:.2f}")
        net_qty = buy_qty - sell_qty
        print(f"  Net inventory: {net_qty:+.4f} HYPE  (${net_qty * mid:+.2f})")
        if avg_buy and avg_sell:
            rtrip = (avg_sell - avg_buy) / avg_buy * 10000
            print(f"  Round-trip spread earned: {rtrip:+.2f} bps  (positive = making money)")
        for f in buy_fills:
            dist = (mid - f["price"]) / mid * 10000
            taker = "market-order/taker!" if f["limit"] == 0.0 else "maker"
            print(f"  BUY  {f['ts'].strftime('%H:%M:%S')}  {f['size']:.4f} @ {f['price']:.4f}  "
                  f"dist={dist:+.2f}bps  [{taker}]")
        for f in sell_fills:
            dist = (f["price"] - mid) / mid * 10000
            taker = "market-order/taker!" if f["limit"] == 0.0 else "maker"
            print(f"  SELL {f['ts'].strftime('%H:%M:%S')}  {f['size']:.4f} @ {f['price']:.4f}  "
                  f"dist={dist:+.2f}bps  [{taker}]")

    print(f"\n--- QUOTE PLACEMENT ---")
    if bids_dist:
        print(f"  Avg bid  distance from current mid: {statistics.mean(bids_dist):+.2f} bps")
    if asks_dist:
        print(f"  Avg ask  distance from current mid: {statistics.mean(asks_dist):+.2f} bps")
    print(f"  Config: born_inf=2bps, born_sup=6bps")
    print(f"  Quiet regime (x0.9): effective quotes at ~1.8bps and ~5.4bps from mid")
    print(f"  => Fills require price to sweep 1.8+ bps toward your quote")

    print(f"\n--- POSITION ---")
    hype_pos = [p for p in positions if "HYPE" in p.get("instrument", "")]
    if not hype_pos:
        print("  No open HYPE position")
    for p in hype_pos:
        qty   = float(p.get("size", p.get("net_size", 0)) or 0)
        entry = float(p.get("avg_entry_price", 0) or 0)
        upnl  = float(p.get("unrealized_pnl", 0) or 0)
        rpnl  = float(p.get("realized_pnl", 0) or 0)
        print(f"  qty={qty:+.4f} HYPE  entry={entry:.4f}  "
              f"uPnL={upnl:.4f}  rPnL={rpnl:.4f}")
        if entry and mid:
            move = (mid - entry) / entry * 10000
            print(f"  Price since entry: {move:+.2f} bps")

    print(f"\n--- VERDICT ---")
    fill_pct    = statuses.get("FILLED", 0) / len(orders) * 100
    expired_pct = reasons.get("EXPIRED", 0) / len(orders) * 100

    print(f"  [{'OK  ' if total_fills > 0 else 'INFO'}] Fill rate {fill_pct:.1f}%  --  "
          f"low but expected given born_inf=2bps vs natural BBO=0.28bps.")
    print(f"         Fills only when price sweeps 1.8+ bps, which takes time on a stable market.")

    if expired_pct > 10:
        print(f"  [WARN] {expired_pct:.0f}% orders expired (15s TTL). Cause: min_step_price=0.1 ($0.1=28bps)")
        print(f"         Bot does not re-quote until mid moves $0.1, so orders sit and expire.")
        print(f"         Fix: min_step_price = '0.01'  (will re-quote after ~3bps price move)")
    else:
        print(f"  [OK  ] Expiry rate {expired_pct:.0f}% is fine")

    market_fill = [f for f in buy_fills + sell_fills if f["limit"] == 0.0]
    if market_fill:
        print(f"  [WARN] {len(market_fill)} fill(s) have limit_price=0.0 -- these look like taker/market orders.")
        print(f"         If the bot placed a market order, verify why (emergency? circuit-breaker?)")
    else:
        print(f"  [OK  ] All fills are maker orders (limit_price > 0)")

    if avg_buy and avg_sell:
        rtrip = (avg_sell - avg_buy) / avg_buy * 10000
        if rtrip > 0:
            print(f"  [OK  ] Positive round-trip spread {rtrip:+.2f} bps -- earning spread, working correctly")
        else:
            print(f"  [WARN] Negative round-trip spread {rtrip:+.2f} bps -- losing on round-trips")

    print(f"  [INFO] v0=0.20 HYPE gives ~$7 base order. With volume_factor ~3x, fills ~$10-20/fill.")
    print(f"         To increase fill frequency: lower born_inf (tighter quotes) or increase max_position.")
    print(f"         To increase fill size:      increase v0 (e.g. '1.0')")


if __name__ == "__main__":
    asyncio.run(main())
