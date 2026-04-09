#!/usr/bin/env python3
import argparse
import json
import math
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any, Dict, List, Optional


def post_json(base_url: str, path: str, body: Dict[str, Any]) -> Dict[str, Any]:
    url = base_url.rstrip("/") + path
    payload = json.dumps(body).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=payload,
        headers={
            "Content-Type": "application/json",
            "Accept": "application/json",
            "User-Agent": "market-making-grvt-analyzer/1.0",
        },
        method="POST",
    )

    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            raw = response.read().decode("utf-8")
            return json.loads(raw)
    except urllib.error.HTTPError as exc:
        body_text = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"{path} HTTP {exc.code}: {body_text}") from exc
    except urllib.error.URLError as exc:
        raise RuntimeError(f"{path} request failed: {exc}") from exc


def parse_float(value: Any) -> Optional[float]:
    if value is None:
        return None
    try:
        return float(str(value))
    except (TypeError, ValueError):
        return None


def internal_symbol(symbol: str) -> str:
    return symbol.replace("_Perp", "-P").replace("_", "/")


@dataclass
class PairRow:
    symbol: str
    grvt_instrument: str
    avg_spread_bps: float
    earnable_spread_bps: float
    trade_count: int
    trade_notional_usd: float
    open_interest: float
    flow_imbalance: float
    last_price: Optional[float]
    funding_rate: Optional[float]
    min_notional: Optional[str]


def get_trade_history_rows(
    base_url: str,
    instrument: str,
    start_time_ns: str,
    end_time_ns: str,
    limit: int,
    max_pages: int,
    verbose: bool,
) -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []
    cursor = ""

    for page in range(max_pages):
        body = {
            "instrument": instrument,
            "start_time": start_time_ns,
            "end_time": end_time_ns,
            "limit": limit,
            "cursor": cursor,
        }
        response = post_json(base_url, "/full/v1/trade_history", body)
        page_rows = response.get("result") or []
        next_cursor = response.get("next") or ""

        if verbose:
            print(
                f"[debug] trade_history instrument={instrument} page={page + 1} rows={len(page_rows)} next_cursor={'yes' if next_cursor else 'no'}"
            )

        rows.extend(page_rows)
        if not next_cursor:
            break
        cursor = str(next_cursor)

    return rows


def get_ticker(base_url: str, instrument: str, derived: bool, verbose: bool) -> Dict[str, Any]:
    body: Dict[str, Any] = {"instrument": instrument}
    if derived:
        body["derived"] = True
    response = post_json(base_url, "/full/v1/ticker", body)
    if verbose:
        print(f"[debug] ticker instrument={instrument} derived={derived} ok")
    return response.get("result") or {}


def min_max(values: List[float]) -> (float, float):
    if not values:
        return 0.0, 0.0
    return min(values), max(values)


def normalize_desc(value: float, low: float, high: float) -> float:
    if high <= low:
        return 1.0
    return 1.0 - ((value - low) / (high - low))


def normalize_asc(value: float, low: float, high: float) -> float:
    if high <= low:
        return 0.0
    return (value - low) / (high - low)


def normalize_log_asc(value: float, low: float, high: float) -> float:
    if value <= 0 or high <= low:
        return 0.0
    log_low = math.log1p(max(low, 0.0))
    log_high = math.log1p(max(high, 0.0))
    log_value = math.log1p(max(value, 0.0))
    if log_high <= log_low:
        return 0.0
    return (log_value - log_low) / (log_high - log_low)


def analyze_pairs(args: argparse.Namespace) -> List[Dict[str, Any]]:
    print("Fetching GRVT active perpetual instruments...")
    instrument_response = post_json(
        args.base_url,
        "/full/v1/all_instruments",
        {"kind": ["PERPETUAL"], "is_active": True, "limit": 1000},
    )
    instruments = [
        item
        for item in (instrument_response.get("result") or [])
        if item.get("kind") == "PERPETUAL" and "ORDERBOOK" in (item.get("venues") or [])
    ]

    if not instruments:
        raise RuntimeError("No active GRVT perpetual ORDERBOOK instruments found.")

    now_ns = int(time.time() * 1_000_000_000)
    start_ns = now_ns - (args.lookback_minutes * 60 * 1_000_000_000)

    print(
        f"Analyzing {len(instruments)} GRVT perpetual pairs over the last {args.lookback_minutes} minutes using {args.mode}..."
    )

    rows: List[PairRow] = []
    failures = 0

    for instrument in instruments:
        instrument_name = instrument["instrument"]

        try:
            ticker = get_ticker(args.base_url, instrument_name, derived=True, verbose=args.verbose)
        except Exception as exc:  # noqa: BLE001
            print(f"Warning: ticker failed for {instrument_name}: {exc}", file=sys.stderr)
            failures += 1
            continue

        best_bid = parse_float(ticker.get("best_bid_price"))
        best_ask = parse_float(ticker.get("best_ask_price"))
        mark_price = parse_float(ticker.get("mark_price"))
        last_price = parse_float(ticker.get("last_price"))
        open_interest = parse_float(ticker.get("open_interest")) or 0.0
        funding_rate = parse_float(ticker.get("funding_rate"))

        if best_bid is None or best_ask is None or best_ask <= best_bid:
            if args.verbose:
                print(f"[debug] skip {instrument_name}: invalid bid/ask", file=sys.stderr)
            continue

        mid = (best_bid + best_ask) / 2.0
        if mid <= 0:
            continue
        spread_bps = ((best_ask - best_bid) / mid) * 10000.0
        earnable_spread_bps = max(spread_bps - args.min_earnable_spread_bps, 0.0)

        trade_count = 0
        trade_notional_usd = 0.0
        flow_imbalance = 0.0

        if args.mode == "trade_history":
            try:
                trades = get_trade_history_rows(
                    args.base_url,
                    instrument_name,
                    str(start_ns),
                    str(now_ns),
                    args.trade_history_limit,
                    args.max_trade_history_pages,
                    args.verbose,
                )
            except Exception as exc:  # noqa: BLE001
                print(f"Warning: trade_history failed for {instrument_name}: {exc}", file=sys.stderr)
                failures += 1
                continue

            taker_buy_notional = 0.0
            taker_sell_notional = 0.0

            for trade in trades:
                price = parse_float(trade.get("price"))
                size = parse_float(trade.get("size"))
                if price is None or size is None:
                    continue

                notional = price * size
                trade_count += 1
                trade_notional_usd += notional
                if trade.get("is_taker_buyer") is True:
                    taker_buy_notional += notional
                else:
                    taker_sell_notional += notional

            if trade_notional_usd > 0:
                flow_imbalance = abs((taker_buy_notional - taker_sell_notional) / trade_notional_usd)
        else:
            buy_volume_24h_q = parse_float(ticker.get("buy_volume_24h_q")) or 0.0
            sell_volume_24h_q = parse_float(ticker.get("sell_volume_24h_q")) or 0.0
            trade_notional_usd = buy_volume_24h_q + sell_volume_24h_q
            if trade_notional_usd > 0:
                flow_imbalance = abs((buy_volume_24h_q - sell_volume_24h_q) / trade_notional_usd)

        rows.append(
            PairRow(
                symbol=internal_symbol(instrument_name),
                grvt_instrument=instrument_name,
                avg_spread_bps=round(spread_bps, 4),
                earnable_spread_bps=round(earnable_spread_bps, 4),
                trade_count=trade_count,
                trade_notional_usd=round(trade_notional_usd, 2),
                open_interest=open_interest,
                flow_imbalance=round(flow_imbalance, 4),
                last_price=last_price or mark_price,
                funding_rate=funding_rate,
                min_notional=instrument.get("min_notional"),
            )
        )

    if not rows:
        raise RuntimeError(
            f"No market-data samples collected from GRVT using mode={args.mode}. Failures={failures}."
        )

    spread_min, spread_max = min_max([row.avg_spread_bps for row in rows])
    earnable_spread_min, earnable_spread_max = min_max([row.earnable_spread_bps for row in rows])
    trade_count_min, trade_count_max = min_max([float(row.trade_count) for row in rows])
    notional_min, notional_max = min_max([row.trade_notional_usd for row in rows])
    oi_min, oi_max = min_max([row.open_interest for row in rows])
    flow_min, flow_max = min_max([row.flow_imbalance for row in rows])

    ranked: List[Dict[str, Any]] = []
    for row in rows:
        spread_tightness_norm = normalize_desc(row.avg_spread_bps, spread_min, spread_max)
        earnable_spread_norm = normalize_asc(row.earnable_spread_bps, earnable_spread_min, earnable_spread_max)
        trade_count_norm = normalize_log_asc(float(row.trade_count), trade_count_min, trade_count_max)
        notional_norm = normalize_log_asc(row.trade_notional_usd, notional_min, notional_max)
        oi_norm = normalize_log_asc(row.open_interest, oi_min, oi_max)
        flow_balance_norm = normalize_desc(row.flow_imbalance, flow_min, flow_max)

        score = 100.0 * (
            (0.30 * earnable_spread_norm)
            + (0.20 * spread_tightness_norm)
            + (0.20 * notional_norm)
            + (0.10 * trade_count_norm)
            + (0.10 * oi_norm)
            + (0.10 * flow_balance_norm)
        )

        if score >= 70:
            recommendation = "strong candidate"
        elif score >= 50:
            recommendation = "worth testing"
        elif score >= 30:
            recommendation = "maybe with tuning"
        else:
            recommendation = "poor current fit"

        ranked.append(
            {
                "Symbol": row.symbol,
                "Score": round(score, 2),
                "Recommendation": recommendation,
                "AvgSpreadBps": row.avg_spread_bps,
                "EarnableSpreadBps": row.earnable_spread_bps,
                "TradeCount": row.trade_count,
                "TradeNotionalUsd": row.trade_notional_usd,
                "OpenInterest": round(row.open_interest, 2),
                "FlowImbalance": row.flow_imbalance,
                "LastPrice": row.last_price,
                "FundingRate": row.funding_rate,
                "MinNotional": row.min_notional,
            }
        )

    ranked.sort(key=lambda item: (item["Score"], item["TradeNotionalUsd"]), reverse=True)
    return ranked


def print_table(rows: List[Dict[str, Any]], top_n: int) -> None:
    selected = rows[:top_n]
    headers = [
        "Symbol",
        "Score",
        "Recommendation",
        "AvgSpreadBps",
        "EarnableSpreadBps",
        "TradeCount",
        "TradeNotionalUsd",
        "OpenInterest",
        "FlowImbalance",
        "LastPrice",
        "FundingRate",
        "MinNotional",
    ]

    widths = {
        header: max(len(header), max((len(str(row.get(header, ""))) for row in selected), default=0))
        for header in headers
    }

    header_line = "  ".join(header.ljust(widths[header]) for header in headers)
    print(header_line)
    print("  ".join("-" * widths[header] for header in headers))
    for row in selected:
        print("  ".join(str(row.get(header, "")).ljust(widths[header]) for header in headers))


def write_csv(path: str, rows: List[Dict[str, Any]]) -> None:
    import csv

    if not rows:
        return

    with open(path, "w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0].keys()))
        writer.writeheader()
        writer.writerows(rows)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Analyze GRVT perpetual pairs for market-making suitability.")
    parser.add_argument("--base-url", default="https://market-data.grvt.io")
    parser.add_argument("--mode", choices=["trade_history", "ticker"], default="trade_history")
    parser.add_argument("--lookback-minutes", type=int, default=1440)
    parser.add_argument("--trade-history-limit", type=int, default=500)
    parser.add_argument("--max-trade-history-pages", type=int, default=20)
    parser.add_argument("--min-earnable-spread-bps", type=float, default=2.0)
    parser.add_argument("--top-n", type=int, default=20)
    parser.add_argument("--out-csv", default="")
    parser.add_argument("--verbose", action="store_true")
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    try:
        ranked = analyze_pairs(args)
    except Exception as exc:  # noqa: BLE001
        print(f"Error: {exc}", file=sys.stderr)
        return 1

    print_table(ranked, args.top_n)
    print("")
    if args.mode == "trade_history":
        print("Scoring notes:")
        print("- Uses GRVT REST endpoints: all_instruments, ticker, and trade_history.")
        print("- TradeCount and TradeNotionalUsd come from the requested trade_history lookback window.")
    else:
        print("Scoring notes:")
        print("- Uses GRVT REST endpoints: all_instruments and ticker with derived 24h fields.")
        print("- TradeNotionalUsd comes from GRVT ticker derived fields.")
    print(f"- EarnableSpreadBps = max(AvgSpreadBps - {args.min_earnable_spread_bps:.2f}, 0).")
    print("- Score favors pairs with enough spread to earn, but still with decent liquidity and balanced flow.")
    print("- This is a market-selection screen, not a profitability guarantee.")

    if args.out_csv:
        write_csv(args.out_csv, ranked)
        print(f"Saved CSV to {args.out_csv}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
