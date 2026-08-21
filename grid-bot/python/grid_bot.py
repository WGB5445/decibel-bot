"""Post-only spot and perpetual grid bots for Decibel."""

from __future__ import annotations

import argparse
import asyncio
import logging
import os
import signal
from dataclasses import dataclass
from decimal import ROUND_DOWN, Decimal
from typing import Literal

from aptos_sdk.account import Account
from aptos_sdk.ed25519 import PrivateKey
from decibel import NAMED_CONFIGS, BaseSDKOptions, DecibelWriteDex, PlaceBulkOrdersSuccess
from decibel._utils import FetchError, get_primary_subaccount_addr
from decibel.read import DecibelReadDex, PerpMarket
from dotenv import load_dotenv

Product = Literal["spot", "perp"]
PerpMode = Literal["neutral", "long", "short"]
PriceSource = Literal["depth", "prices"]
RangeMode = Literal["bounds", "percent", "step"]
MAX_BULK_LEVELS_PER_SIDE = 40
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
    dry_run: bool

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
            dry_run=args.dry_run or os.getenv("DRY_RUN", "false").lower() == "true",
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
        if config.total_grid_count is not None and not 2 <= config.total_grid_count <= 80:
            raise ValueError("GRID_TOTAL_COUNT must be between 2 and 80")
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
        """Build a centered grid, with at most 40 price levels on either side.

        Bid vectors are descending and ask vectors ascending, as required by Decibel's Move
        bulk-order validation. A narrower range than the tick size can yield fewer levels.
        """
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

        # Bulk orders do not have a reduce-only flag. Directional perp modes are deliberately
        # single-sided: long places only bids; short places only asks. Neutral is two-sided.
        if config.product == "perp" and config.perp_mode == "long":
            asks = []
        elif config.product == "perp" and config.perp_mode == "short":
            bids = []

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
    """Return requested bid/ask counts before tick rounding.

    GRID_TOTAL_COUNT means the combined number of actual orders. For an odd neutral count,
    the extra order is allocated to asks. Directional perp grids use the whole count on their
    enabled side; a dummy opposite-side level is generated then discarded to reuse validation.
    """
    if config.total_grid_count is None:
        return config.levels_per_side, config.levels_per_side
    if config.product == "perp" and config.perp_mode == "long":
        return config.total_grid_count, 1
    if config.product == "perp" and config.perp_mode == "short":
        return 1, config.total_grid_count
    bid_count = config.total_grid_count // 2
    return bid_count, config.total_grid_count - bid_count


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
    and quote-equivalent base inventory for asks. A perp budget is a conservative
    total margin budget under PREVIEW_LEVERAGE, not a promise of exchange acceptance.
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
        bid_notional = sum(bids, Decimal(0))
        ask_notional = sum(asks, Decimal(0))
        margin_per_base = (
            max(bid_notional, ask_notional) / config.preview_leverage
            + (bid_notional + ask_notional) * maker_fee_rate
        )
        if margin_per_base <= 0:
            raise ValueError("cannot derive size from an empty grid")
        bid_size = ask_size = config.total_budget / margin_per_base

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
    return value.lower().removeprefix("0x").lstrip("0") or "0"


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
        executor = DecibelGridExecution(
            self.write, self.config, self.market, self.subaccount, await self._next_bulk_sequence()
        )
        try:
            while not self.stop_event.is_set():
                try:
                    orders = await self._build_orders()
                    if self.config.product == "perp" and not await self._position_is_safe(orders):
                        LOG.warning(
                            "perp position limit or worst-case grid exposure reached; "
                            "cancelling grid"
                        )
                        await executor.cancel()
                    else:
                        await executor.place(orders)
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

    async def _build_orders(self, maker_fee_rate: Decimal | None = None) -> GridOrders:
        assert self.market is not None
        mid = await self._mid_price()
        fee = maker_fee_rate if maker_fee_rate is not None else await self._maker_fee_rate()
        return GridMath.orders(
            self.config,
            mid,
            chain_units_to_decimal(self.market.tick_size, self.market.px_decimals),
            chain_units_to_decimal(self.market.lot_size, self.market.sz_decimals),
            chain_units_to_decimal(self.market.min_size, self.market.sz_decimals),
            fee,
        )

    async def _mid_price(self) -> Decimal:
        assert self.read is not None and self.market is not None
        if self.config.price_source == "prices":
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
        assets = await self.read.spot_market_assets(self.market.market_addr)
        balances = {
            position.asset_addr.lower(): Decimal(str(position.amount))
            for position in overview.spot.positions
        }
        base_available = balances.get(assets.base_asset_addr.lower(), Decimal(0))
        quote_available = balances.get(assets.quote_asset_addr.lower(), Decimal(0))
        warnings = []
        if quote_available < quote_required:
            warnings.append("INSUFFICIENT QUOTE: not enough quote asset to reserve all spot bids.")
        if base_available < base_required:
            warnings.append("INSUFFICIENT BASE: not enough base asset to reserve all spot asks.")
        return FundingPreview(
            quote_required,
            quote_available,
            base_required,
            base_available,
            None,
            None,
            warnings,
        )

    async def _position_is_safe(self, orders: GridOrders) -> bool:
        assert self.read is not None and self.market is not None and self.subaccount
        if self.config.max_position is None:
            return True
        positions = await self.read.user_positions.get_by_addr(
            sub_addr=self.subaccount, market_addr=self.market.market_addr
        )
        position = next(
            (item for item in positions if item.market == self.market.market_addr), None
        )
        current = Decimal(str(position.size)) if position else Decimal(0)
        worst_long = current + sum(orders.bid_sizes, Decimal(0))
        worst_short = current - sum(orders.ask_sizes, Decimal(0))
        return (
            abs(current) < self.config.max_position
            and abs(worst_long) <= self.config.max_position
            and abs(worst_short) <= self.config.max_position
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
        help="total combined Bid+Ask order count (2-80); overrides --levels-per-side",
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
    args = parser.parse_args()
    return args.product, args


async def main() -> None:
    product, args = parse_args()
    bot = GridBot(GridConfig.from_env_and_args(product, args))
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
