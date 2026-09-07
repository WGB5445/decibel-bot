"""Post-only spot and perpetual grid bots for Decibel."""

from __future__ import annotations

import argparse
import asyncio
import fcntl
import hashlib
import logging
import os
import signal
import sys
import time
from dataclasses import dataclass
from decimal import ROUND_DOWN, Decimal
from pathlib import Path
from typing import Literal

from aptos_sdk.account import Account
from aptos_sdk.ed25519 import PrivateKey
from decibel import NAMED_CONFIGS, BaseSDKOptions, DecibelWriteDex, PlaceBulkOrdersSuccess
from decibel._transaction_builder import InputEntryFunctionData
from decibel._utils import FetchError, get_primary_subaccount_addr
from decibel.read import DecibelReadDex, PerpMarket
from decibel.write import TimeInForce
from dotenv import load_dotenv

Product = Literal["spot", "perp"]
PerpMode = Literal["neutral", "long", "short"]
OutOfRangeAction = Literal["pause", "cancel_orders", "close_position", "clamp_continue"]
PriceSource = Literal["depth", "prices"]
RangeMode = Literal["bounds", "percent", "step"]
MAX_BULK_LEVELS_PER_SIDE = 40
BULK_REPLACEMENT_COOLDOWN_SECONDS = 30.0
MAX_TAKER_FUNDING_ATTEMPTS = 6
PERP_CANCEL_CONFIRM_ATTEMPTS = 6
PERP_CANCEL_CONFIRM_INTERVAL_SECONDS = 1.0
TAKER_SLIPPAGE = Decimal("0.003")
TAKER_FEE_BUFFER = Decimal("0.001")
LOG = logging.getLogger("decibel_grid")


@dataclass(frozen=True)
class GridConfig:
    private_key: str | None
    decibel_api_key: str | None
    aptos_node_api_key: str | None
    subaccount_address: str | None
    network: str
    product: Product
    perp_mode: PerpMode
    market: str
    lower_price: Decimal | None
    upper_price: Decimal | None
    range_percent: Decimal | None
    grid_step_percent: Decimal | None
    levels_per_side: int
    total_grid_count: int | None
    order_size: Decimal | None
    total_budget: Decimal | None
    refresh_seconds: float
    max_position: Decimal | None
    maker_fee_rate: Decimal | None
    preview_leverage: Decimal
    price_source: PriceSource
    out_of_range_action: OutOfRangeAction
    dry_run: bool
    log_file: str | None = None
    spot_funding_amount: Decimal | None = None
    spot_funding_metadata: str | None = None

    @classmethod
    def from_env_and_args(cls, product: Product, args: argparse.Namespace) -> GridConfig:
        def decimal_value(name: str, value: str | None, *, positive: bool = True) -> Decimal:
            if value is None:
                raise ValueError(f"{name} is required")
            try:
                parsed = Decimal(value)
            except Exception as exc:
                raise ValueError(f"{name} must be a decimal number") from exc
            if (positive and parsed <= 0) or (not positive and parsed < 0):
                comparison = "positive" if positive else "zero or positive"
                raise ValueError(f"{name} must be {comparison}")
            return parsed

        def first_set(
            value: object | None, env_name: str, default: str | None = None
        ) -> object | None:
            return value if value is not None else os.getenv(env_name, default)

        def optional_decimal_value(name: str, value: object | None) -> Decimal | None:
            return decimal_value(name, str(value)) if value is not None else None

        range_from_cli = any(
            value is not None
            for value in (
                args.lower_price,
                args.upper_price,
                args.range_percent,
                args.grid_step_percent,
            )
        )
        lower_value = args.lower_price if range_from_cli else os.getenv("GRID_LOWER_PRICE")
        upper_value = args.upper_price if range_from_cli else os.getenv("GRID_UPPER_PRICE")
        range_percent_value = (
            args.range_percent if range_from_cli else os.getenv("GRID_RANGE_PERCENT")
        )
        step_percent_value = (
            args.grid_step_percent if range_from_cli else os.getenv("GRID_STEP_PERCENT")
        )
        budget_from_cli = args.order_size is not None or args.total_budget is not None
        order_size_value = args.order_size if budget_from_cli else os.getenv("GRID_ORDER_SIZE")
        total_budget_value = (
            args.total_budget if budget_from_cli else os.getenv("GRID_TOTAL_BUDGET")
        )

        maker_fee = first_set(args.maker_fee_rate, "GRID_MAKER_FEE_RATE")
        decibel_api_key = first_set(args.decibel_api_key, "DECIBEL_API_KEY")
        if not decibel_api_key:
            raise ValueError(
                "DECIBEL_API_KEY is required; set it in .env or pass --decibel-api-key"
            )
        config = cls(
            private_key=os.getenv("PRIVATE_KEY"),
            decibel_api_key=str(decibel_api_key),
            aptos_node_api_key=first_set(args.aptos_node_api_key, "APTOS_NODE_API_KEY")
            or os.getenv("NODE_API_KEY"),
            subaccount_address=first_set(args.subaccount, "SUBACCOUNT_ADDRESS") or None,
            network=str(first_set(args.network, "NETWORK", "testnet")),
            product=product,
            perp_mode=str(first_set(args.perp_mode, "PERP_GRID_MODE", "neutral")),
            market=str(first_set(args.market, "MARKET", "BTC/USD")),
            lower_price=optional_decimal_value("GRID_LOWER_PRICE", lower_value),
            upper_price=optional_decimal_value("GRID_UPPER_PRICE", upper_value),
            range_percent=optional_decimal_value("GRID_RANGE_PERCENT", range_percent_value),
            grid_step_percent=optional_decimal_value("GRID_STEP_PERCENT", step_percent_value),
            levels_per_side=int(first_set(args.levels_per_side, "GRID_LEVELS_PER_SIDE", "40")),
            total_grid_count=(
                int(first_set(args.grid_count, "GRID_TOTAL_COUNT"))
                if first_set(args.grid_count, "GRID_TOTAL_COUNT") is not None
                else None
            ),
            order_size=optional_decimal_value("GRID_ORDER_SIZE", order_size_value),
            total_budget=optional_decimal_value("GRID_TOTAL_BUDGET", total_budget_value),
            refresh_seconds=float(first_set(args.refresh_seconds, "GRID_REFRESH_SECONDS", "20")),
            max_position=(
                decimal_value(
                    "GRID_MAX_POSITION", first_set(args.max_position, "GRID_MAX_POSITION")
                )
                if first_set(args.max_position, "GRID_MAX_POSITION") is not None
                else None
            ),
            maker_fee_rate=(
                decimal_value("GRID_MAKER_FEE_RATE", maker_fee, positive=False)
                if maker_fee is not None
                else None
            ),
            preview_leverage=decimal_value(
                "PREVIEW_LEVERAGE",
                args.preview_leverage or os.getenv("PREVIEW_LEVERAGE", "1"),
            ),
            price_source=str(first_set(args.price_source, "PRICE_SOURCE", "depth")),
            out_of_range_action=str(
                first_set(args.out_of_range_action, "GRID_OUT_OF_RANGE_ACTION", "pause")
            ),
            dry_run=args.dry_run or os.getenv("DRY_RUN", "false").lower() == "true",
            log_file=(
                str(first_set(args.log_file, "LOG_FILE"))
                if first_set(args.log_file, "LOG_FILE")
                else None
            ),
            spot_funding_amount=optional_decimal_value(
                "SPOT_FUNDING_AMOUNT",
                first_set(args.spot_funding_amount, "SPOT_FUNDING_AMOUNT"),
            ),
            spot_funding_metadata=str(
                first_set(args.spot_funding_metadata, "SPOT_FUNDING_METADATA")
            ) if first_set(args.spot_funding_metadata, "SPOT_FUNDING_METADATA") else None,
        )
        if config.levels_per_side < 1 or config.levels_per_side > MAX_BULK_LEVELS_PER_SIDE:
            raise ValueError(
                f"GRID_LEVELS_PER_SIDE must be between 1 and {MAX_BULK_LEVELS_PER_SIDE}"
            )
        range_modes = sum(
            (
                config.lower_price is not None or config.upper_price is not None,
                config.range_percent is not None,
                config.grid_step_percent is not None,
            )
        )
        if config.lower_price is None != config.upper_price is None:
            raise ValueError("GRID_LOWER_PRICE and GRID_UPPER_PRICE must be provided together")
        if range_modes != 1:
            raise ValueError(
                "choose exactly one range mode: lower/upper prices, GRID_RANGE_PERCENT, "
                "or GRID_STEP_PERCENT"
            )
        if config.lower_price is not None and config.lower_price >= config.upper_price:
            raise ValueError("GRID_LOWER_PRICE must be less than GRID_UPPER_PRICE")
        if config.total_grid_count is not None and not 2 <= config.total_grid_count <= 40:
            raise ValueError("GRID_TOTAL_COUNT must be between 2 and 40")
        if config.out_of_range_action not in (
            "pause",
            "cancel_orders",
            "close_position",
            "clamp_continue",
        ):
            raise ValueError(
                "GRID_OUT_OF_RANGE_ACTION must be pause, cancel_orders, close_position, "
                "or clamp_continue"
            )
        if config.range_percent is not None and config.range_percent >= 100:
            raise ValueError("GRID_RANGE_PERCENT must be below 100")
        if config.grid_step_percent is not None and config.grid_step_percent >= 100:
            raise ValueError("GRID_STEP_PERCENT must be below 100")
        if (config.order_size is None) == (config.total_budget is None):
            raise ValueError("provide exactly one of GRID_ORDER_SIZE or GRID_TOTAL_BUDGET")
        if config.refresh_seconds <= 0:
            raise ValueError("GRID_REFRESH_SECONDS must be positive")
        if config.maker_fee_rate is not None and config.maker_fee_rate >= 1:
            raise ValueError("GRID_MAKER_FEE_RATE must be a decimal rate below 1, e.g. 0.0001")
        if config.price_source not in ("depth", "prices"):
            raise ValueError("PRICE_SOURCE must be 'depth' or 'prices'")
        if product == "spot" and config.perp_mode != "neutral":
            raise ValueError("PERP_GRID_MODE applies only to perp grids")
        if product == "spot" and config.max_position is not None:
            LOG.warning("GRID_MAX_POSITION applies only to perpetual grids and will be ignored")
        return config


@dataclass(frozen=True)
class GridOrders:
    bid_prices: list[Decimal]
    bid_sizes: list[Decimal]
    ask_prices: list[Decimal]
    ask_sizes: list[Decimal]


@dataclass(frozen=True)
class PerpOutOfRangeDecision:
    raw_planning_price: Decimal
    effective_planning_price: Decimal
    action: OutOfRangeAction
    direction: Literal["below", "above"] | None
    skip_bulk: bool
    paused: bool
    cancel_orders: bool
    close_position: bool


def perp_out_of_range_decision(
    config: GridConfig, planning_price: Decimal
) -> PerpOutOfRangeDecision:
    """Resolve one unified Perp out-of-range action without implicit clamping."""
    total = config.total_grid_count or config.levels_per_side * 2
    lower, upper = resolve_range(config, planning_price, total)
    direction: Literal["below", "above"] | None = None
    if planning_price < lower:
        direction = "below"
    elif planning_price > upper:
        direction = "above"
    if direction is None:
        return PerpOutOfRangeDecision(
            planning_price, planning_price, config.out_of_range_action,
            None, False, False, False, False,
        )
    if config.out_of_range_action == "clamp_continue":
        return PerpOutOfRangeDecision(
            planning_price, min(max(planning_price, lower), upper),
            config.out_of_range_action, direction, False, False, False, False,
        )
    return PerpOutOfRangeDecision(
        planning_price,
        planning_price,
        config.out_of_range_action,
        direction,
        True,
        config.out_of_range_action == "pause",
        config.out_of_range_action in ("cancel_orders", "close_position"),
        config.out_of_range_action == "close_position",
    )


@dataclass(frozen=True)
class SpotFunds:
    base_balance: Decimal
    quote_balance: Decimal
    base_reserved: Decimal = Decimal(0)
    quote_reserved: Decimal = Decimal(0)
    quote_cross_balance: Decimal = Decimal(0)

    @property
    def available_base(self) -> Decimal:
        return max(self.base_balance, Decimal(0))

    @property
    def available_quote(self) -> Decimal:
        # Bulk orders can source only from the PFS, never Cross/collateral.
        return max(self.quote_balance, Decimal(0))

    @property
    def available_base_for_bulk(self) -> Decimal:
        return max(self.base_balance + self.base_reserved, Decimal(0))

    @property
    def available_quote_for_bulk(self) -> Decimal:
        return max(self.quote_balance + self.quote_reserved, Decimal(0))


@dataclass(frozen=True)
class ProfitPreview:
    pair_count: int
    gross_profit: Decimal
    maker_fees: Decimal
    net_profit: Decimal
    min_pair_net_profit: Decimal
    max_pair_net_profit: Decimal


@dataclass(frozen=True)
class FundingPreview:
    required_quote: Decimal
    available_quote: Decimal | None
    required_base: Decimal
    available_base: Decimal | None
    required_margin: Decimal | None
    available_margin: Decimal | None
    warnings: list[str]


def fit_spot_orders_to_funds(
    orders: GridOrders, funds: SpotFunds, maker_fee_rate: Decimal = Decimal(0)
) -> GridOrders:
    """Keep the nearest levels that the PFS can fund, preserving ABI ordering."""
    bid_count = 0
    quote_used = Decimal(0)
    for price, size in zip(orders.bid_prices, orders.bid_sizes, strict=True):
        cost = price * size * (Decimal(1) + maker_fee_rate)
        if quote_used + cost > funds.available_quote_for_bulk:
            break
        quote_used += cost
        bid_count += 1
    ask_count = 0
    base_used = Decimal(0)
    for size in orders.ask_sizes:
        if base_used + size > funds.available_base_for_bulk:
            break
        base_used += size
        ask_count += 1
    return GridOrders(
        orders.bid_prices[:bid_count], orders.bid_sizes[:bid_count],
        orders.ask_prices[:ask_count], orders.ask_sizes[:ask_count],
    )


class GridMath:
    @staticmethod
    def orders(
        config: GridConfig,
        mid: Decimal,
        tick_size: Decimal,
        lot_size: Decimal,
        min_size: Decimal,
        maker_fee_rate: Decimal = Decimal(0),
    ) -> GridOrders:
        """Build a centered grid with bilateral Perp inclusion or Spot side allocation."""
        if config.product == "perp":
            return build_perp_orders(
                config, mid, tick_size, lot_size, min_size, maker_fee_rate
            )
        bid_count, ask_count = grid_side_counts(config)
        lower, upper = resolve_range(config, mid, max(bid_count, ask_count))
        if not lower < mid < upper:
            raise ValueError(
                f"mid price {mid} is outside the configured grid range [{lower}, {upper}]"
            )

        if config.grid_step_percent is not None:
            step = config.grid_step_percent / Decimal(100)
            bids = unique_prices(
                [
                    quantize_down(mid * ((Decimal(1) - step) ** i), tick_size)
                    for i in range(1, bid_count + 1)
                ],
                descending=True,
            )
            asks = unique_prices(
                [
                    quantize_down(mid * ((Decimal(1) + step) ** i), tick_size)
                    for i in range(1, ask_count + 1)
                ],
                descending=False,
            )
        else:
            bids = unique_prices(
                [
                    quantize_down(
                        mid - (mid - lower) * Decimal(i) / bid_count,
                        tick_size,
                    )
                    for i in range(1, bid_count + 1)
                ],
                descending=True,
            )
            asks = unique_prices(
                [
                    quantize_down(
                        mid + (upper - mid) * Decimal(i) / ask_count,
                        tick_size,
                    )
                    for i in range(1, ask_count + 1)
                ],
                descending=False,
            )
        if not bids or not asks:
            raise ValueError("grid range is too narrow for the market tick size")
        if len(bids) > MAX_BULK_LEVELS_PER_SIDE or len(asks) > MAX_BULK_LEVELS_PER_SIDE:
            raise ValueError(
                f"a Decibel bulk order supports at most {MAX_BULK_LEVELS_PER_SIDE} levels per side"
            )

        bid_size, ask_size = order_sizes_for_budget(
            config, mid, bids, asks, lot_size, min_size, maker_fee_rate
        )
        return GridOrders([*bids], [bid_size] * len(bids), [*asks], [ask_size] * len(asks))

    @staticmethod
    def funding_requirements(
        orders: GridOrders, maker_fee_rate: Decimal, product: Product, leverage: Decimal
    ) -> tuple[Decimal, Decimal, Decimal | None]:
        """Return quote needed for bids, base needed for asks, and worst-side perp margin."""
        quote = sum(
            (
                price * size * (Decimal(1) + maker_fee_rate)
                for price, size in zip(orders.bid_prices, orders.bid_sizes, strict=True)
            ),
            Decimal(0),
        )
        base = sum(orders.ask_sizes, Decimal(0))
        if product == "spot":
            return quote, base, None
        long_notional = sum(
            (price * size for price, size in zip(orders.bid_prices, orders.bid_sizes, strict=True)),
            Decimal(0),
        )
        short_notional = sum(
            (price * size for price, size in zip(orders.ask_prices, orders.ask_sizes, strict=True)),
            Decimal(0),
        )
        # Conservative preview: reserve the larger one-sided notional / configured leverage,
        # plus maker fees. Actual required margin is exchange/account-state dependent.
        return (
            quote,
            base,
            max(long_notional, short_notional) / leverage
            + (long_notional + short_notional) * maker_fee_rate,
        )

    @staticmethod
    def profit_preview(orders: GridOrders, maker_fee_rate: Decimal) -> ProfitPreview:
        """Estimate maker-to-maker grid capture for executable two-sided adjacent pairs.

        Each pair assumes a buy fills at a bid and the same base quantity later sells at the
        next ask. It excludes funding, price drift, liquidation, gas, and partial fills.
        """
        pairs = min(len(orders.bid_prices), len(orders.ask_prices))
        if pairs == 0:
            return ProfitPreview(0, Decimal(0), Decimal(0), Decimal(0), Decimal(0), Decimal(0))
        gross = Decimal(0)
        fees = Decimal(0)
        net_pairs: list[Decimal] = []
        for index in range(pairs):
            buy_price, sell_price = orders.bid_prices[index], orders.ask_prices[index]
            size = min(orders.bid_sizes[index], orders.ask_sizes[index])
            buy_notional, sell_notional = buy_price * size, sell_price * size
            pair_gross = sell_notional - buy_notional
            pair_fees = (buy_notional + sell_notional) * maker_fee_rate
            gross += pair_gross
            fees += pair_fees
            net_pairs.append(pair_gross - pair_fees)
        return ProfitPreview(pairs, gross, fees, gross - fees, min(net_pairs), max(net_pairs))


def grid_side_counts(config: GridConfig) -> tuple[int, int]:
    """Return requested bid/ask counts before tick rounding for Spot grids."""
    if config.total_grid_count is None:
        return config.levels_per_side, config.levels_per_side
    bid_count = config.total_grid_count // 2
    return bid_count, config.total_grid_count - bid_count


def uniform_range_prices(
    lower: Decimal, upper: Decimal, count: int, tick_size: Decimal
) -> list[Decimal]:
    if count == 0:
        return []
    span = upper - lower
    if span <= 0:
        raise ValueError("grid lower bound must be below upper bound")
    if count == 1:
        prices = [quantize_down(lower + span / Decimal(2), tick_size)]
    else:
        # `count` is the requested number of price points, not intervals.
        denom = Decimal(count - 1)
        prices = [
            quantize_down(lower + span * Decimal(index) / denom, tick_size)
            for index in range(count)
        ]
    prices = sorted(set(prices))
    if len(prices) != count:
        raise ValueError(
            f"grid range is too narrow for {count} distinct price points at "
            f"market tick size {tick_size}"
        )
    return prices


def split_uniform_levels(
    config: GridConfig,
    lower: Decimal,
    upper: Decimal,
    planning_price: Decimal,
    tick_size: Decimal,
) -> tuple[list[Decimal], list[Decimal]]:
    total = config.total_grid_count or config.levels_per_side * 2
    prices = uniform_range_prices(lower, upper, total, tick_size)
    bids: list[Decimal] = []
    asks: list[Decimal] = []
    for price in prices:
        if abs(price - planning_price) <= tick_size / 2:
            continue
        if price < planning_price and price not in asks:
            bids.append(price)
        elif price > planning_price and price not in bids:
            asks.append(price)
    return sorted(bids, reverse=True), sorted(asks)


def compute_perp_target(
    perp_mode: PerpMode, ask_levels: int, bid_levels: int, grid_size: Decimal
) -> Decimal:
    if perp_mode == "long":
        return Decimal(ask_levels) * grid_size
    if perp_mode == "short":
        return -Decimal(bid_levels) * grid_size
    return Decimal(ask_levels - bid_levels) * grid_size / 2


def perp_theoretical_limits(
    config: GridConfig, ask_levels: int, bid_levels: int, grid_size: Decimal
) -> tuple[Decimal, Decimal]:
    if config.max_position is not None:
        return config.max_position, config.max_position
    total = ask_levels + bid_levels
    if config.perp_mode == "long":
        return Decimal(total) * grid_size, Decimal(0)
    if config.perp_mode == "short":
        return Decimal(0), Decimal(total) * grid_size
    max_side = Decimal(total) * grid_size / 2
    return max_side, max_side


def perp_worst_case(position: Decimal, orders: GridOrders) -> tuple[Decimal, Decimal]:
    bid_sum = sum(orders.bid_sizes, Decimal(0))
    ask_sum = sum(orders.ask_sizes, Decimal(0))
    return position + bid_sum, position - ask_sum


def perp_position_is_safe(
    config: GridConfig,
    position: Decimal,
    orders: GridOrders,
) -> bool:
    ask_levels = len(orders.ask_prices)
    bid_levels = len(orders.bid_prices)
    grid_size = orders.bid_sizes[0] if orders.bid_sizes else (
        orders.ask_sizes[0] if orders.ask_sizes else Decimal(0)
    )
    max_long, max_short = perp_theoretical_limits(config, ask_levels, bid_levels, grid_size)
    worst_long, worst_short = perp_worst_case(position, orders)
    if config.perp_mode == "long":
        return worst_short >= 0 and worst_long <= max_long
    if config.perp_mode == "short":
        return worst_long <= 0 and worst_short >= -max_short
    return worst_long <= max_long and worst_short >= -max_short


def build_perp_orders(
    config: GridConfig,
    planning_price: Decimal,
    tick_size: Decimal,
    lot_size: Decimal,
    min_size: Decimal,
    maker_fee_rate: Decimal,
) -> GridOrders:
    decision = perp_out_of_range_decision(config, planning_price)
    total = config.total_grid_count or config.levels_per_side * 2
    lower, upper = resolve_range(config, planning_price, total)
    if decision.skip_bulk:
        return GridOrders([], [], [], [])
    planning_price = decision.effective_planning_price
    bids, asks = split_uniform_levels(config, lower, upper, planning_price, tick_size)
    if len(bids) > MAX_BULK_LEVELS_PER_SIDE or len(asks) > MAX_BULK_LEVELS_PER_SIDE:
        raise ValueError(
            f"a Decibel bulk order supports at most {MAX_BULK_LEVELS_PER_SIDE} levels per side"
        )
    bid_size, ask_size = order_sizes_for_budget(
        config, planning_price, bids, asks, lot_size, min_size, maker_fee_rate
    )
    grid_size = bid_size if bids else ask_size
    return GridOrders([*bids], [grid_size] * len(bids), [*asks], [grid_size] * len(asks))


def trim_perp_pending_to_risk(
    config: GridConfig, position: Decimal, orders: GridOrders
) -> GridOrders:
    """Trim farthest pending levels only; the caller's target remains unchanged."""
    bids = list(zip(orders.bid_prices, orders.bid_sizes, strict=True))
    asks = list(zip(orders.ask_prices, orders.ask_sizes, strict=True))
    while bids or asks:
        candidate = GridOrders(
            [price for price, _ in bids],
            [size for _, size in bids],
            [price for price, _ in asks],
            [size for _, size in asks],
        )
        if perp_position_is_safe(config, position, candidate):
            return candidate
        grid_size = (
            candidate.bid_sizes[0]
            if candidate.bid_sizes
            else candidate.ask_sizes[0] if candidate.ask_sizes else Decimal(0)
        )
        max_long, max_short = perp_theoretical_limits(
            config, len(candidate.ask_prices), len(candidate.bid_prices), grid_size
        )
        worst_long, worst_short = perp_worst_case(position, candidate)
        long_violation = worst_long > max_long or (
            config.perp_mode == "short" and worst_long > 0
        )
        short_violation = worst_short < -max_short or (
            config.perp_mode == "long" and worst_short < 0
        )
        if long_violation and bids:
            bids.pop()  # descending bids: remove farthest/lowest first
        elif short_violation and asks:
            asks.pop()  # ascending asks: remove farthest/highest first
        elif bids:
            bids.pop()
        elif asks:
            asks.pop()
    return GridOrders([], [], [], [])


def resolve_range(
    config: GridConfig, mid: Decimal, max_side_levels: int
) -> tuple[Decimal, Decimal]:
    """Resolve fixed bounds, a symmetric percentage band, or compounded step spacing."""
    if config.lower_price is not None and config.upper_price is not None:
        return config.lower_price, config.upper_price
    if config.range_percent is not None:
        fraction = config.range_percent / Decimal(100)
        return mid * (Decimal(1) - fraction), mid * (Decimal(1) + fraction)
    assert config.grid_step_percent is not None
    fraction = config.grid_step_percent / Decimal(100)
    return (
        mid * ((Decimal(1) - fraction) ** max_side_levels),
        mid * ((Decimal(1) + fraction) ** max_side_levels),
    )


def order_sizes_for_budget(
    config: GridConfig,
    mid: Decimal,
    bids: list[Decimal],
    asks: list[Decimal],
    lot_size: Decimal,
    min_size: Decimal,
    maker_fee_rate: Decimal,
) -> tuple[Decimal, Decimal]:
    """Derive a uniform size for each side, or validate an explicit fixed size.

    A spot budget is split 50/50 between quote reserved for bids (fee buffer included)
    and quote-equivalent base inventory for asks. A perp budget covers the maximum position
    after all levels fill on both sides plus pending notional for fees, using
    total_levels × representative_price (matching Rust `derive_perp_grid_size`).
    """
    if config.order_size is not None:
        size = quantize_down(config.order_size, lot_size)
        if size <= 0:
            raise ValueError("GRID_ORDER_SIZE rounds to zero at this market's lot size")
        if size < min_size:
            raise ValueError(
                f"GRID_ORDER_SIZE rounds to {size}, below the market minimum size {min_size}"
            )
        return size, size

    assert config.total_budget is not None
    if config.product == "spot":
        half_budget = config.total_budget / 2
        bid_denominator = sum(bids, Decimal(0)) * (Decimal(1) + maker_fee_rate)
        # Value the required ask inventory at its own submitted ask prices so the two
        # planned order sides each consume no more than half of the total budget.
        ask_denominator = sum(asks, Decimal(0))
        bid_size = half_budget / bid_denominator if bids else Decimal(0)
        ask_size = half_budget / ask_denominator if asks else Decimal(0)
    else:
        total_levels = len(bids) + len(asks)
        if total_levels == 0:
            raise ValueError("cannot derive size from an empty grid")
        representative_price = asks[0] if asks else bids[-1]
        denominator = (
            Decimal(total_levels) * representative_price / config.preview_leverage
            + Decimal(total_levels) * representative_price * maker_fee_rate
        )
        if denominator <= 0:
            raise ValueError("cannot derive size from an empty grid")
        bid_size = ask_size = config.total_budget / denominator

    bid_size = quantize_down(bid_size, lot_size) if bids else Decimal(0)
    ask_size = quantize_down(ask_size, lot_size) if asks else Decimal(0)
    if bids and bid_size < min_size:
        raise ValueError(
            "GRID_TOTAL_BUDGET is too small: the derived bid size is below the market minimum size"
        )
    if asks and ask_size < min_size:
        raise ValueError(
            "GRID_TOTAL_BUDGET is too small: the derived ask size is below the market minimum size"
        )
    return bid_size, ask_size


def unique_prices(prices: list[Decimal], *, descending: bool) -> list[Decimal]:
    return sorted(set(prices), reverse=descending)


def quantize_down(value: Decimal, increment: Decimal) -> Decimal:
    return (value / increment).to_integral_value(rounding=ROUND_DOWN) * increment


def scale_to_chain_units(value: Decimal, decimals: int) -> int:
    return int(value * (Decimal(10) ** decimals))


def chain_units_to_decimal(value: int | float, decimals: int) -> Decimal:
    return Decimal(str(value)) / (Decimal(10) ** decimals)


def normalize_addr(value: str) -> str:
    return value.strip().lower().removeprefix("0x").lstrip("0") or "0"


def compute_spot_taker_funding(
    funds: SpotFunds,
    orders: GridOrders,
    best_ask: Decimal,
    tick_size: Decimal,
    lot_size: Decimal,
    min_size: Decimal,
) -> tuple[Decimal, Decimal, Decimal]:
    """Return (base_gap, limit_price, IOC quantity) without consuming bid reserve."""
    if best_ask <= 0:
        raise ValueError("best ask must be positive for Spot IOC funding")
    required_quote = sum(
        (price * size for price, size in zip(orders.bid_prices, orders.bid_sizes, strict=True)),
        Decimal(0),
    )
    base_gap = max(sum(orders.ask_sizes, Decimal(0)) - funds.available_base_for_bulk, Decimal(0))
    quote_surplus = min(
        max(funds.available_quote_for_bulk - required_quote, Decimal(0)),
        funds.available_quote,
    )
    limit_price = quantize_down(best_ask * (Decimal(1) + TAKER_SLIPPAGE), tick_size)
    affordable = quote_surplus / (limit_price * (Decimal(1) + TAKER_FEE_BUFFER))
    quantity = quantize_down(min(base_gap, affordable), lot_size)
    if quantity < min_size:
        quantity = Decimal(0)
    return base_gap, limit_price, quantity


class SubaccountRunLock:
    """Non-blocking process lock shared by all markets on one subaccount."""

    def __init__(self, network: str, subaccount: str) -> None:
        root = Path(os.getenv("DECIBEL_GRID_DATA_DIR", Path.home() / ".local" / "share"))
        lock_dir = root / "decibel-grid" / "locks"
        lock_dir.mkdir(parents=True, exist_ok=True)
        key = f"{network.strip().lower()}:{normalize_addr(subaccount)}".encode()
        path = lock_dir / f"subaccount-{hashlib.sha256(key).hexdigest()}.lock"
        self._file = path.open("a+")
        try:
            fcntl.flock(self._file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            self._file.close()
            raise RuntimeError(
                f"another grid process is already running for network {network} and this "
                "subaccount; stop it before starting a second instance"
            ) from exc

    def __enter__(self) -> SubaccountRunLock:
        return self

    def __exit__(self, *_: object) -> None:
        fcntl.flock(self._file.fileno(), fcntl.LOCK_UN)
        self._file.close()


class DecibelGridExecution:
    def __init__(
        self,
        write: DecibelWriteDex,
        config: GridConfig,
        market: PerpMarket,
        subaccount: str,
        sequence: int,
    ) -> None:
        self.write, self.config, self.market, self.subaccount = write, config, market, subaccount
        self.sequence = sequence - 1

    async def cancel(self) -> None:
        if self.config.dry_run:
            LOG.info("[dry-run] would cancel current %s grid", self.config.product)
            return
        if self.config.product == "spot":
            await self.write.cancel_spot_bulk_order(
                market_addr=self.market.market_addr, subaccount_addr=self.subaccount
            )
        else:
            await self.write.cancel_bulk_order(
                market_name=self.market.market_name, subaccount_addr=self.subaccount
            )

    async def place(self, orders: GridOrders) -> bool:
        self.sequence += 1
        if self.config.dry_run:
            LOG.info(
                "[dry-run] %s grid seq=%d: %d bids; %d asks",
                self.config.product,
                self.sequence,
                len(orders.bid_prices),
                len(orders.ask_prices),
            )
            return True
        kwargs = {
            "sequence_number": self.sequence,
            "bid_prices": [
                scale_to_chain_units(item, self.market.px_decimals) for item in orders.bid_prices
            ],
            "bid_sizes": [
                scale_to_chain_units(item, self.market.sz_decimals) for item in orders.bid_sizes
            ],
            "ask_prices": [
                scale_to_chain_units(item, self.market.px_decimals) for item in orders.ask_prices
            ],
            "ask_sizes": [
                scale_to_chain_units(item, self.market.sz_decimals) for item in orders.ask_sizes
            ],
            "subaccount_addr": self.subaccount,
        }
        if self.config.product == "spot":
            result = await self.write.place_spot_bulk_order(
                market_addr=self.market.market_addr, **kwargs
            )
            LOG.info("placed spot grid seq=%d tx=%s", self.sequence, result.get("hash", ""))
            return True
        result = await self.write.place_bulk_orders(market_name=self.market.market_name, **kwargs)
        if isinstance(result, PlaceBulkOrdersSuccess):
            LOG.info("placed perp grid seq=%d tx=%s", self.sequence, result.transaction_hash)
            return True
        LOG.error("perp grid placement failed: %s", result.error)
        return False


class GridBot:
    def __init__(self, config: GridConfig) -> None:
        self.config, self.stop_event = config, asyncio.Event()
        self.read: DecibelReadDex | None = None
        self.write: DecibelWriteDex | None = None
        self.market: PerpMarket | None = None
        self.subaccount: str | None = None

    async def initialize(self, *, with_write: bool) -> None:
        network = NAMED_CONFIGS.get(self.config.network)
        if network is None:
            raise ValueError(
                f"unknown NETWORK {self.config.network!r}; expected one of {list(NAMED_CONFIGS)}"
            )
        account = (
            Account.load_key(PrivateKey.from_hex(self.config.private_key).hex())
            if self.config.private_key
            else None
        )
        if with_write and account is None:
            raise ValueError("PRIVATE_KEY is required to run the bot")
        self.subaccount = self.config.subaccount_address or (
            get_primary_subaccount_addr(
                str(account.address()), network.compat_version, network.deployment.package
            )
            if account is not None
            else None
        )
        self.read = DecibelReadDex(network, api_key=self.config.decibel_api_key)
        if with_write:
            assert account is not None
            self.write = DecibelWriteDex(
                network,
                account,
                opts=BaseSDKOptions(node_api_key=self.config.aptos_node_api_key, no_fee_payer=True),
            )
        markets = await self.read.markets.get_all(include_spot=True)
        self.market = next(
            (
                item
                for item in markets
                if item.market_name == self.config.market
                and (item.asset_type.value if item.asset_type else "perp") == self.config.product
            ),
            None,
        )
        if self.market is None:
            raise ValueError(f"{self.config.product} market {self.config.market!r} was not found")

    async def preview(self) -> None:
        await self.initialize(with_write=False)
        try:
            maker_fee = await self._maker_fee_rate()
            orders = await self._build_orders(maker_fee)
            estimate = GridMath.profit_preview(orders, maker_fee)
            funding = await self._funding_preview(orders, maker_fee)
            print_preview(self.config, self.market, orders, maker_fee, estimate, funding)
        finally:
            assert self.read is not None
            await self.read.close()

    async def start(self) -> None:
        await self.initialize(with_write=True)
        assert (
            self.read is not None
            and self.write is not None
            and self.market is not None
            and self.subaccount
        )
        # One process lock per (network, subaccount), independent of market, so two processes
        # cannot race bulk sequence numbers or Spot funding for the same subaccount.
        with SubaccountRunLock(self.config.network, self.subaccount):
            await self._run_loop()

    async def _run_loop(self) -> None:
        assert (
            self.read is not None
            and self.write is not None
            and self.market is not None
            and self.subaccount
        )
        if self.config.product == "spot" and not self.config.dry_run:
            await self._transfer_cross_to_pfs()
        executor = DecibelGridExecution(
            self.write, self.config, self.market, self.subaccount, await self._next_bulk_sequence()
        )
        # One-shot Spot base funding: only ever attempted once per process, and only before
        # any bulk ladder is resting, since re-buying after each sell fill would hand back the
        # captured spread plus taker fees.
        spot_base_funded = self.config.product != "spot" or self.config.dry_run
        last_bulk_replacement_at: float | None = None
        last_submitted_level_count: int | None = None
        try:
            while not self.stop_event.is_set():
                try:
                    if not spot_base_funded:
                        spot_base_funded = True
                        try:
                            await self._fund_spot_base_if_needed()
                        except Exception:
                            LOG.exception("Spot base funding failed; ask side will be shrunk")
                    planning_price = await self._mid_price()
                    if self.config.product == "perp":
                        decision = perp_out_of_range_decision(self.config, planning_price)
                        if decision.direction is not None:
                            LOG.warning(
                                "Perp planning price %s is %s the configured range; "
                                "GRID_OUT_OF_RANGE_ACTION=%s",
                                decision.raw_planning_price,
                                decision.direction,
                                decision.action,
                            )
                        if decision.skip_bulk:
                            if decision.cancel_orders:
                                await executor.cancel()
                            if decision.close_position:
                                if decision.cancel_orders:
                                    await self._wait_for_bulk_orders_cleared()
                                await self._close_perp_position_ioc()
                            await asyncio.sleep(self.config.refresh_seconds)
                            continue
                    orders = await self._build_orders(planning_price=planning_price)
                    if self.config.product == "spot":
                        funds = await self._spot_funds()
                        fee = await self._maker_fee_rate()
                        fitted = fit_spot_orders_to_funds(orders, funds, fee)
                        if len(fitted.bid_prices) < len(orders.bid_prices) or len(
                            fitted.ask_prices
                        ) < len(orders.ask_prices):
                            LOG.warning(
                                "Spot PFS limited the grid: %d bid(s) + %d ask(s) fit "
                                "(planned %d + %d)",
                                len(fitted.bid_prices),
                                len(fitted.ask_prices),
                                len(orders.bid_prices),
                                len(orders.ask_prices),
                            )
                        orders = fitted
                    if self.config.product == "perp" and not await self._position_is_safe(orders):
                        LOG.warning(
                            "perp position limit or worst-case grid exposure reached; "
                            "cancelling grid"
                        )
                        await executor.cancel()
                    else:
                        level_count = len(orders.bid_prices) + len(orders.ask_prices)
                        structural_change = (
                            last_submitted_level_count is None
                            or last_submitted_level_count != level_count
                        )
                        cooldown_active = (
                            last_bulk_replacement_at is not None
                            and (time.monotonic() - last_bulk_replacement_at)
                            < BULK_REPLACEMENT_COOLDOWN_SECONDS
                        )
                        if await self._bulk_matches(orders):
                            LOG.info(
                                "bulk ladder already matches desired levels; replacement skipped"
                            )
                            last_bulk_replacement_at = time.monotonic()
                            last_submitted_level_count = level_count
                        elif cooldown_active and not structural_change:
                            LOG.info(
                                "bulk replacement skipped: minor ladder drift during %ds "
                                "cooldown (%d desired levels; no level-count change)",
                                int(BULK_REPLACEMENT_COOLDOWN_SECONDS),
                                level_count,
                            )
                        else:
                            await executor.place(orders)
                            last_bulk_replacement_at = time.monotonic()
                            last_submitted_level_count = level_count
                except Exception:
                    LOG.exception("grid refresh failed")
                    if self.config.product == "perp" and not self.config.dry_run:
                        try:
                            await executor.cancel()
                        except Exception:
                            LOG.exception("failed to cancel perp grid after refresh failure")
                try:
                    await asyncio.wait_for(
                        self.stop_event.wait(), timeout=self.config.refresh_seconds
                    )
                except TimeoutError:
                    pass
        finally:
            if not self.config.dry_run:
                try:
                    await executor.cancel()
                except Exception:
                    LOG.exception("failed to cancel grid during shutdown")
            await self.read.close()

    async def _transfer_cross_to_pfs(self) -> None:
        """Move an explicitly requested USDC amount from Cross into Spot PFS.

        HOLD_AS_NON_COLLATERAL is deliberately not changed here: that Move entry function is
        owner-only and a trading signer/delegate cannot safely submit it on the operator's behalf.
        """
        assert self.write is not None and self.subaccount
        amount = self.config.spot_funding_amount
        metadata = self.config.spot_funding_metadata
        if amount is None:
            return
        if not metadata:
            raise ValueError(
                "SPOT_FUNDING_METADATA/--spot-funding-metadata is required with "
                "SPOT_FUNDING_AMOUNT"
            )
        if amount < 0:
            raise ValueError("SPOT_FUNDING_AMOUNT cannot be negative")
        raw_amount = int((amount * Decimal(1_000_000)).to_integral_value(rounding=ROUND_DOWN))
        if raw_amount <= 0:
            raise ValueError("SPOT_FUNDING_AMOUNT is below one USDC micro-unit")
        network = NAMED_CONFIGS[self.config.network]
        package = network.deployment.package
        payload = InputEntryFunctionData(
            function=f"{package}::dex_accounts_entry::transfer_assets_between_non_collateral_and_collateral",
            type_arguments=[],
            function_arguments=[self.subaccount, metadata, -raw_amount],
        )
        LOG.warning(
            "NOTICE: HOLD_AS_NON_COLLATERAL is owner-only and is not submitted by this bot. "
            "Set it manually in the Decibel UI/wallet before relying on future Spot proceeds "
            "staying in PFS."
        )
        LOG.info("Transferring %s USDC from Cross to PFS", amount)
        result = await self.write._send_tx(payload)  # type: ignore[attr-defined]
        LOG.info("Cross to PFS transfer submitted: tx=%s", result.get("hash", ""))

    async def _fund_spot_base_if_needed(self) -> None:
        """Buy any Spot base shortfall with bounded IOC orders before the first placement."""
        assert (
            self.read is not None
            and self.write is not None
            and self.market is not None
            and self.subaccount
        )
        orders = await self._build_orders()
        funds = await self._spot_funds()
        ask_total = sum(orders.ask_sizes, Decimal(0))
        base_gap = max(ask_total - funds.available_base_for_bulk, Decimal(0))
        if base_gap <= 0:
            return
        tick = chain_units_to_decimal(self.market.tick_size, self.market.px_decimals)
        lot = chain_units_to_decimal(self.market.lot_size, self.market.sz_decimals)
        min_size = chain_units_to_decimal(self.market.min_size, self.market.sz_decimals)
        LOG.info(
            "Spot base funding: plan needs %s base, PFS holds %s, buying up to %s with IOC orders.",
            sum(orders.ask_sizes, Decimal(0)),
            funds.available_base_for_bulk,
            base_gap,
        )
        for attempt in range(1, MAX_TAKER_FUNDING_ATTEMPTS + 1):
            depth = await self.read.market_depth.get_by_addr(self.market.market_addr, limit=1)
            if not depth.asks:
                raise ValueError(f"Spot order book for {self.market.market_name} has no ask")
            best_ask = Decimal(str(depth.asks[0].price))
            remaining_gap, limit_price, quantity = compute_spot_taker_funding(
                funds, orders, best_ask, tick, lot, min_size
            )
            if remaining_gap <= 0:
                return
            if quantity <= 0:
                LOG.info(
                    "Spot IOC funding stopped with %s base remaining; below minimum order "
                    "size %s at %s quote.",
                    remaining_gap,
                    min_size,
                    limit_price,
                )
                return
            # The SDK's spot order writer accepts chain units, unlike the pure Decimal
            # planning code above. Keep the IOC bound aligned with the market tick/lot ABI.
            result = await self.write.place_spot_order(
                price=scale_to_chain_units(limit_price, self.market.px_decimals),
                size=scale_to_chain_units(quantity, self.market.sz_decimals),
                is_buy=True,
                time_in_force=TimeInForce.ImmediateOrCancel,
                market_addr=self.market.market_addr,
                subaccount_addr=self.subaccount,
                tick_size=self.market.tick_size,
            )
            LOG.info(
                "  IOC %d/%d: buy up to %s at limit %s, tx=%s",
                attempt,
                MAX_TAKER_FUNDING_ATTEMPTS,
                quantity,
                limit_price,
                getattr(result, "transaction_hash", ""),
            )
            funds = await self._spot_funds()
            remaining = max(ask_total - funds.available_base_for_bulk, Decimal(0))
            LOG.info(
                "  filled to %s base; %s still needed", funds.available_base_for_bulk, remaining
            )
            if remaining <= 0:
                return
        LOG.warning(
            "Spot funding stopped short of the planned asks after %d IOC attempts; "
            "the ask side will be shrunk to fit.",
            MAX_TAKER_FUNDING_ATTEMPTS,
        )

    async def _build_orders(
        self,
        maker_fee_rate: Decimal | None = None,
        planning_price: Decimal | None = None,
    ) -> GridOrders:
        assert self.market is not None
        mid = planning_price if planning_price is not None else await self._mid_price()
        fee = maker_fee_rate if maker_fee_rate is not None else await self._maker_fee_rate()
        return GridMath.orders(
            self.config,
            mid,
            chain_units_to_decimal(self.market.tick_size, self.market.px_decimals),
            chain_units_to_decimal(self.market.lot_size, self.market.sz_decimals),
            chain_units_to_decimal(self.market.min_size, self.market.sz_decimals),
            fee,
        )

    async def _wait_for_bulk_orders_cleared(self) -> None:
        """Poll until no active bulk ladder remains before reduce-only closes."""
        assert self.read is not None and self.market is not None and self.subaccount
        if self.config.dry_run:
            return
        for attempt in range(1, PERP_CANCEL_CONFIRM_ATTEMPTS + 1):
            rows = await self.read.user_bulk_orders.get_by_addr(
                sub_addr=self.subaccount,
                market=self.market.market_addr,
                asset_type=self.config.product,
            )
            active = [row for row in rows if row.cancellation_reason is None]
            if not active:
                return
            if attempt == PERP_CANCEL_CONFIRM_ATTEMPTS:
                raise ValueError(
                    f"Perp bulk cancellation was committed but {len(active)} order(s) "
                    f"remain active after {PERP_CANCEL_CONFIRM_ATTEMPTS} confirmation "
                    "attempts; refusing market close"
                )
            await asyncio.sleep(PERP_CANCEL_CONFIRM_INTERVAL_SECONDS)

    async def _close_perp_position_ioc(self) -> None:
        """Cancel-first, reduce-only IOC flatten for close_position OOR action."""
        assert self.read is not None and self.write is not None
        assert self.market is not None and self.subaccount
        positions = await self.read.user_positions.get_by_addr(
            sub_addr=self.subaccount, market_addr=self.market.market_addr
        )
        position = next(
            (item for item in positions if item.market == self.market.market_addr), None
        )
        current = Decimal(str(position.size)) if position else Decimal(0)
        if current == 0:
            return
        if self.config.dry_run:
            LOG.info("[dry-run] would IOC-flatten Perp position %s", current)
            return
        depth = await self.read.market_depth.get_by_addr(self.market.market_addr, limit=1)
        is_buy = current < 0
        levels = depth.asks if is_buy else depth.bids
        if not levels:
            raise ValueError("cannot close Perp position: executable book side is empty")
        reference = Decimal(str(levels[0].price))
        raw_price = reference * (Decimal("1.003") if is_buy else Decimal("0.997"))
        tick = chain_units_to_decimal(self.market.tick_size, self.market.px_decimals)
        lot = chain_units_to_decimal(self.market.lot_size, self.market.sz_decimals)
        price = quantize_down(raw_price, tick)
        size = quantize_down(abs(current), lot)
        if size <= 0:
            return
        result = await self.write.place_order(
            market_name=self.market.market_name,
            price=scale_to_chain_units(price, self.market.px_decimals),
            size=scale_to_chain_units(size, self.market.sz_decimals),
            is_buy=is_buy,
            time_in_force=TimeInForce.ImmediateOrCancel,
            is_reduce_only=True,
            subaccount_addr=self.subaccount,
            tick_size=self.market.tick_size,
        )
        LOG.info(
            "Perp out-of-range close IOC submitted: position=%s tx=%s",
            current,
            getattr(result, "transaction_hash", ""),
        )

    async def _mid_price(self) -> Decimal:
        assert self.read is not None and self.market is not None
        # `/prices` is a perpetual feed. Spot always uses its live order book,
        # even when PRICE_SOURCE=prices was inherited from a perp configuration.
        if self.config.product != "spot" and self.config.price_source == "prices":
            return await self._mid_from_prices()
        try:
            depth = await self.read.market_depth.get_by_addr(self.market.market_addr, limit=1)
        except FetchError as exc:
            if exc.status == 404:
                raise ValueError(
                    f"Decibel returned 404 for the order book of {self.market.market_name} "
                    f"({self.market.market_addr}) on network '{self.config.network}'. This "
                    "market row came from /markets, so the market exists; the /depth route "
                    "also exists (an anonymous request returns 401, not 404). So the server "
                    "has no order book for this specific market address. Most likely this "
                    "market is not quoting on this network, or this deployment's /depth does "
                    f"not serve '{self.config.product}' markets. Verify on the Decibel UI "
                    "that the market has a live book. As a last resort, "
                    "'--price-source prices' quotes from the /prices mid_px instead -- read "
                    "the README caveats first; it is not the live book."
                ) from exc
            raise
        if not depth.bids or not depth.asks:
            raise ValueError("cannot build a grid: order book has no bid and ask")
        return (Decimal(str(depth.bids[0].price)) + Decimal(str(depth.asks[0].price))) / 2

    async def _mid_from_prices(self) -> Decimal:
        """Mid from the /prices feed. Explicitly opt-in: it is not the live order book."""
        assert self.read is not None and self.market is not None
        prices = await self.read.market_prices.get_all()
        wanted = normalize_addr(self.market.market_addr)
        price = next((item for item in prices if normalize_addr(item.market) == wanted), None)
        if price is None:
            raise ValueError(
                f"/prices returned no row for {self.market.market_name} "
                f"({self.market.market_addr}); it may not cover this product."
            )
        if price.mid_px <= 0:
            raise ValueError(
                f"/prices reported a non-positive mid_px ({price.mid_px}) for "
                f"{self.market.market_name}; refusing to build a grid from it."
            )
        LOG.warning(
            "quoting from /prices mid_px=%s for %s; this is not the live order book",
            price.mid_px,
            self.market.market_name,
        )
        return Decimal(str(price.mid_px))

    async def _maker_fee_rate(self) -> Decimal:
        if self.config.maker_fee_rate is not None:
            return self.config.maker_fee_rate
        if self.subaccount is None:
            raise ValueError(
                "preview without PRIVATE_KEY or SUBACCOUNT_ADDRESS requires --maker-fee-rate "
                "or GRID_MAKER_FEE_RATE"
            )
        assert self.read is not None
        fees = await self.read.user_fees.get_by_addr(self.subaccount)
        state = fees.spot if self.config.product == "spot" else fees.perp
        return Decimal(str(state.user_maker_rate if state is not None else fees.user_maker_rate))

    async def _funding_preview(self, orders: GridOrders, maker_fee: Decimal) -> FundingPreview:
        quote_required, base_required, margin_required = GridMath.funding_requirements(
            orders, maker_fee, self.config.product, self.config.preview_leverage
        )
        if self.subaccount is None:
            return FundingPreview(
                quote_required,
                None,
                base_required,
                None,
                margin_required,
                None,
                ["Balance check skipped: set SUBACCOUNT_ADDRESS or PRIVATE_KEY to inspect funds."],
            )
        assert self.read is not None and self.market is not None
        overview = await self.read.account_overview.get_by_addr(sub_addr=self.subaccount)
        if self.config.product == "perp":
            available = (
                Decimal(str(overview.cross_available_to_trade))
                if overview.cross_available_to_trade is not None
                else None
            )
            warnings = []
            if available is None:
                warnings.append("Margin availability is unavailable from the account overview.")
            elif margin_required is not None and available < margin_required:
                warnings.append(
                    "INSUFFICIENT MARGIN: available margin is below the conservative "
                    "grid requirement."
                )
            return FundingPreview(
                quote_required,
                None,
                base_required,
                None,
                margin_required,
                available,
                warnings,
            )

        if overview.spot is None:
            return FundingPreview(
                quote_required,
                None,
                base_required,
                None,
                None,
                None,
                ["Spot inventory is unavailable; cannot verify funds."],
            )
        funds = await self._spot_funds(overview)
        warnings = []
        if funds.available_quote_for_bulk < quote_required:
            warnings.append(
                "INSUFFICIENT PFS QUOTE: Cross/collateral USDC cannot fund Spot bulk bids; "
                "transfer it to PFS first."
            )
        if funds.available_base_for_bulk < base_required:
            warnings.append("INSUFFICIENT BASE: not enough base asset to reserve all spot asks.")
        if funds.quote_cross_balance > 0:
            warnings.append(
                f"Cross USDC {funds.quote_cross_balance:.8f} is diagnostic only; "
                "the bot cannot use it directly for Spot bulk bids."
            )
        return FundingPreview(
            quote_required,
            funds.available_quote_for_bulk,
            base_required,
            funds.available_base_for_bulk,
            None,
            None,
            warnings,
        )

    async def _spot_funds(self, overview: object | None = None) -> SpotFunds:
        assert self.read is not None and self.market is not None and self.subaccount
        if overview is None:
            overview = await self.read.account_overview.get_by_addr(sub_addr=self.subaccount)
        spot = getattr(overview, "spot", None)
        if spot is None:
            raise ValueError("Spot inventory is unavailable from account overview")
        assets = await self.read.spot_market_assets(self.market.market_addr)
        base_addr = normalize_addr(assets.base_asset_addr)
        quote_addr = normalize_addr(assets.quote_asset_addr)
        positions = {
            normalize_addr(position.asset_addr): Decimal(str(position.amount))
            for position in spot.positions
        }
        base_reserved = Decimal(0)
        quote_reserved = Decimal(0)
        for order in spot.in_flight_orders:
            amount = max(Decimal(str(order.reserved_amount)), Decimal(0))
            if normalize_addr(order.reserved_asset) == base_addr:
                base_reserved += amount
            elif normalize_addr(order.reserved_asset) == quote_addr:
                quote_reserved += amount
        return SpotFunds(
            base_balance=positions.get(base_addr, Decimal(0)),
            quote_balance=positions.get(quote_addr, Decimal(0)),
            base_reserved=base_reserved,
            quote_reserved=quote_reserved,
            quote_cross_balance=max(
                Decimal(str(getattr(overview, "usdc_cross_withdrawable_balance", 0) or 0)),
                Decimal(0),
            ),
        )

    async def _position_is_safe(self, orders: GridOrders) -> bool:
        assert self.read is not None and self.market is not None and self.subaccount
        positions = await self.read.user_positions.get_by_addr(
            sub_addr=self.subaccount, market_addr=self.market.market_addr
        )
        position = next(
            (item for item in positions if item.market == self.market.market_addr), None
        )
        current = Decimal(str(position.size)) if position else Decimal(0)
        return perp_position_is_safe(self.config, current, orders)

    async def _bulk_matches(self, orders: GridOrders) -> bool:
        """Reconcile the desired ladder against the latest active bulk row."""
        assert self.read is not None and self.market is not None and self.subaccount
        rows = await self.read.user_bulk_orders.get_by_addr(
            sub_addr=self.subaccount,
            market=self.market.market_addr,
            asset_type=self.config.product,
        )
        active = [row for row in rows if row.cancellation_reason is None]
        if not active:
            return False
        row = max(active, key=lambda item: item.sequence_number)

        def same(values: list[float], wanted: list[Decimal]) -> bool:
            pairs = zip(values, wanted, strict=True)
            return len(values) == len(wanted) and all(
                Decimal(str(actual)) == expected for actual, expected in pairs
            )

        return (
            same(row.bid_prices, orders.bid_prices)
            and same(row.bid_sizes, orders.bid_sizes)
            and same(row.ask_prices, orders.ask_prices)
            and same(row.ask_sizes, orders.ask_sizes)
        )

    async def _next_bulk_sequence(self) -> int:
        assert self.read is not None and self.market is not None and self.subaccount
        existing = await self.read.user_bulk_orders.get_by_addr(
            sub_addr=self.subaccount, market=self.market.market_addr, asset_type=self.config.product
        )
        return max((item.sequence_number for item in existing), default=0) + 1

    def stop(self) -> None:
        self.stop_event.set()


def print_grid_levels(orders: GridOrders) -> None:
    """Print the final tick/lot-aligned orders the bot will submit."""
    print("Grid orders (price × size):")
    for index, (price, size) in enumerate(
        zip(orders.bid_prices, orders.bid_sizes, strict=True), start=1
    ):
        print(f"  BID {index:>2}: {price:.8f} × {size:.8f} = {price * size:.8f}")
    for index, (price, size) in enumerate(
        zip(orders.ask_prices, orders.ask_sizes, strict=True), start=1
    ):
        print(f"  ASK {index:>2}: {price:.8f} × {size:.8f} = {price * size:.8f}")


def print_preview(
    config: GridConfig,
    market: PerpMarket | None,
    orders: GridOrders,
    maker_fee: Decimal,
    estimate: ProfitPreview,
    funding: FundingPreview,
) -> None:
    assert market is not None
    print(f"Product / market: {config.product} / {market.market_name}")
    print(f"Perp mode: {config.perp_mode}" if config.product == "perp" else "Mode: spot")
    print(
        f"Bulk levels: {len(orders.bid_prices)} bids + {len(orders.ask_prices)} asks "
        f"(limit: {MAX_BULK_LEVELS_PER_SIDE}/side)"
    )
    if config.total_grid_count is not None:
        print(f"Requested total grid orders: {config.total_grid_count}")
    if config.total_budget is not None:
        print(f"Requested total budget: {config.total_budget:.8f} quote/USDC")
    print(f"Maker fee: {maker_fee:.8%}")
    print_grid_levels(orders)
    print(f"Estimated matched maker cycles: {estimate.pair_count}")
    print(f"Gross grid capture: {estimate.gross_profit:.8f} quote")
    print(f"Estimated maker fees: {estimate.maker_fees:.8f} quote")
    print(f"Estimated net grid capture: {estimate.net_profit:.8f} quote")
    if estimate.pair_count:
        print(
            "Net per matched pair: "
            f"{estimate.min_pair_net_profit:.8f} to {estimate.max_pair_net_profit:.8f} quote"
        )
    else:
        print("No two-sided matched cycles in this directional grid; profit estimate is N/A.")
    print(f"Required quote for bids: {funding.required_quote:.8f}")
    if funding.available_quote is not None:
        print(f"Available quote: {funding.available_quote:.8f}")
    print(f"Required base for asks: {funding.required_base:.8f}")
    if funding.available_base is not None:
        print(f"Available base: {funding.available_base:.8f}")
    if funding.required_margin is not None:
        print(f"Estimated required perp margin: {funding.required_margin:.8f} USDC")
    if funding.available_margin is not None:
        print(f"Available perp margin: {funding.available_margin:.8f} USDC")
    for warning in funding.warnings:
        print(f"WARNING: {warning}")
    if not funding.warnings and (
        funding.available_quote is not None or funding.available_margin is not None
    ):
        print("Funding check: sufficient for the previewed worst-case reservation.")


def redirect_output_to_log(path_value: str) -> None:
    """Truncate a log file and redirect both Python stdout and stderr to it."""
    path = Path(path_value).expanduser()
    path.parent.mkdir(parents=True, exist_ok=True)
    stream = path.open("w", encoding="utf-8")
    sys.stdout = stream
    sys.stderr = stream
    logging.basicConfig(
        level=os.getenv("LOG_LEVEL", "INFO"),
        format="%(asctime)s %(levelname)s %(message)s",
        stream=stream,
        force=True,
    )
    print(f"CLI log started; output is being overwritten at {path}", flush=True)


def parse_args() -> tuple[Product, argparse.Namespace]:
    parser = argparse.ArgumentParser(description="Decibel spot/perpetual grid bot")
    parser.add_argument("product", choices=("spot", "perp"), help="market product")
    parser.add_argument("--network", choices=tuple(NAMED_CONFIGS), help="override NETWORK")
    parser.add_argument("--market", help="override MARKET")
    parser.add_argument(
        "--lower-price", help="fixed lower boundary; use together with --upper-price"
    )
    parser.add_argument(
        "--upper-price", help="fixed upper boundary; use together with --lower-price"
    )
    parser.add_argument(
        "--range-percent",
        help="symmetric percentage range around live mid, e.g. 10 means mid ±10%%",
    )
    parser.add_argument(
        "--grid-step-percent",
        help="percentage spacing between adjacent levels, e.g. 0.5 means 0.5%% per step",
    )
    parser.add_argument("--levels-per-side", type=int, help="legacy per-side count (1-40)")
    parser.add_argument(
        "--grid-count",
        type=int,
        help="total combined Bid+Ask order count (2-40); overrides --levels-per-side",
    )
    parser.add_argument("--order-size", help="fixed size per order in base units")
    parser.add_argument(
        "--total-budget",
        help="total quote/USDC budget; automatically derives uniform order sizes",
    )
    parser.add_argument("--refresh-seconds", type=float, help="override GRID_REFRESH_SECONDS")
    parser.add_argument("--max-position", help="override GRID_MAX_POSITION in base units")
    parser.add_argument("--subaccount", help="override SUBACCOUNT_ADDRESS")
    parser.add_argument(
        "--decibel-api-key", help="override DECIBEL_API_KEY for Decibel REST/WebSocket API"
    )
    parser.add_argument(
        "--aptos-node-api-key", help="override APTOS_NODE_API_KEY for Aptos fullnode RPC"
    )
    parser.add_argument(
        "--perp-mode", choices=("neutral", "long", "short"), help="perp grid direction"
    )
    parser.add_argument(
        "--out-of-range-action",
        choices=("pause", "cancel_orders", "close_position", "clamp_continue"),
        help="Perp range action; default GRID_OUT_OF_RANGE_ACTION or pause",
    )
    parser.add_argument("--maker-fee-rate", help="decimal maker fee override, e.g. 0.0001")
    parser.add_argument(
        "--preview-leverage",
        help="conservative perp-margin estimate leverage; default PREVIEW_LEVERAGE or 1",
    )
    parser.add_argument(
        "--price-source",
        choices=("depth", "prices"),
        help="mid-price source: 'depth' (live order book, default) or 'prices' (/prices mid_px)",
    )
    parser.add_argument(
        "--preview",
        action="store_true",
        help="show live grid, fees, and estimated capture; send no transaction",
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="run refresh loop but send no transactions"
    )
    parser.add_argument(
        "--log-file",
        help="truncate and redirect stdout/stderr to this file (also LOG_FILE)",
    )
    parser.add_argument(
        "--spot-funding-amount",
        help="USDC amount to transfer from Cross to PFS before running Spot",
    )
    parser.add_argument(
        "--spot-funding-metadata",
        help="Spot quote asset metadata address (required with --spot-funding-amount)",
    )
    args = parser.parse_args()
    return args.product, args


async def main() -> None:
    product, args = parse_args()
    config = GridConfig.from_env_and_args(product, args)
    if config.log_file:
        if args.preview:
            raise ValueError("--log-file is not supported with --preview")
        redirect_output_to_log(config.log_file)
    bot = GridBot(config)
    if args.preview:
        await bot.preview()
        return
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, bot.stop)
        except NotImplementedError:
            pass
    await bot.start()


if __name__ == "__main__":
    load_dotenv()
    logging.basicConfig(
        level=os.getenv("LOG_LEVEL", "INFO"), format="%(asctime)s %(levelname)s %(message)s"
    )
    asyncio.run(main())
