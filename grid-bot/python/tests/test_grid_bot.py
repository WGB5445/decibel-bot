from decimal import Decimal

import pytest

from grid_bot import (
    MAX_BULK_LEVELS_PER_SIDE,
    TAKER_FEE_BUFFER,
    GridConfig,
    GridMath,
    GridOrders,
    SpotFunds,
    build_perp_orders,
    chain_units_to_decimal,
    compute_perp_target,
    compute_spot_taker_funding,
    fit_spot_orders_to_funds,
    grid_side_counts,
    perp_position_is_safe,
    perp_out_of_range_decision,
    quantize_down,
    scale_to_chain_units,
    trim_perp_pending_to_risk,
)


def config(**overrides: object) -> GridConfig:
    values: dict[str, object] = {
        "private_key": None,
        "decibel_api_key": "test-decibel-key",
        "aptos_node_api_key": None,
        "subaccount_address": None,
        "network": "testnet",
        "product": "spot",
        "perp_mode": "neutral",
        "market": "BTC/USD",
        "lower_price": Decimal("90"),
        "upper_price": Decimal("110"),
        "range_percent": None,
        "grid_step_percent": None,
        "levels_per_side": 2,
        "total_grid_count": None,
        "order_size": Decimal("0.015"),
        "total_budget": None,
        "refresh_seconds": 20.0,
        "max_position": None,
        "maker_fee_rate": Decimal("0.001"),
        "preview_leverage": Decimal("1"),
        "price_source": "depth",
        "out_of_range_action": "pause",
        "dry_run": True,
    }
    values.update(overrides)
    return GridConfig(**values)  # type: ignore[arg-type]


def grid_orders(**overrides: object):
    return GridMath.orders(
        config(**overrides),
        Decimal("100"),
        Decimal("1"),
        Decimal("0.01"),
        Decimal("0.01"),
    )


def test_quantize_down_never_rounds_up() -> None:
    assert quantize_down(Decimal("100.019"), Decimal("0.01")) == Decimal("100.01")
    assert quantize_down(Decimal("0.019"), Decimal("0.01")) == Decimal("0.01")


def test_chain_unit_scaling_preserves_tick_and_lot_alignment() -> None:
    assert chain_units_to_decimal(25, 2) == Decimal("0.25")
    assert scale_to_chain_units(Decimal("100.25"), 2) == 10025
    assert scale_to_chain_units(Decimal("0.001"), 6) == 1000


def test_grid_has_requested_bid_and_ask_levels_in_required_order() -> None:
    orders = grid_orders()
    assert orders.bid_prices == [Decimal("95"), Decimal("90")]
    assert orders.ask_prices == [Decimal("105"), Decimal("110")]
    assert orders.bid_sizes == [Decimal("0.01"), Decimal("0.01")]
    assert orders.ask_sizes == [Decimal("0.01"), Decimal("0.01")]


def test_total_grid_count_is_combined_bid_and_ask_orders() -> None:
    orders = grid_orders(total_grid_count=5)
    assert len(orders.bid_prices) == 2
    assert len(orders.ask_prices) == 3
    assert len(orders.bid_prices) + len(orders.ask_prices) == 5
    assert grid_side_counts(config(total_grid_count=5)) == (2, 3)


def test_percent_range_is_resolved_around_live_mid() -> None:
    orders = grid_orders(
        lower_price=None,
        upper_price=None,
        range_percent=Decimal("10"),
        total_grid_count=4,
    )
    assert orders.bid_prices == [Decimal("95"), Decimal("90")]
    assert orders.ask_prices == [Decimal("105"), Decimal("110")]


def test_step_percent_creates_compounded_spacing() -> None:
    orders = grid_orders(
        lower_price=None,
        upper_price=None,
        grid_step_percent=Decimal("1"),
        total_grid_count=4,
    )
    assert orders.bid_prices == [Decimal("99"), Decimal("98")]
    assert orders.ask_prices == [Decimal("101"), Decimal("102")]


def test_spot_budget_auto_sizes_each_side_using_half_budget() -> None:
    orders = grid_orders(total_budget=Decimal("400"), order_size=None)
    # Bid budget includes 0.1% maker fee: 200 / ((95 + 90) * 1.001) -> 1.08 lots.
    assert orders.bid_sizes == [Decimal("1.08"), Decimal("1.08")]
    # Ask half-budget uses the submitted ask values: 200 / (105 + 110) -> 0.93 lots.
    assert orders.ask_sizes == [Decimal("0.93"), Decimal("0.93")]


def test_40_per_side_grid_is_supported() -> None:
    orders = GridMath.orders(
        config(levels_per_side=MAX_BULK_LEVELS_PER_SIDE),
        Decimal("100"),
        Decimal("0.1"),
        Decimal("0.01"),
        Decimal("0.01"),
    )
    assert len(orders.bid_prices) == MAX_BULK_LEVELS_PER_SIDE
    assert len(orders.ask_prices) == MAX_BULK_LEVELS_PER_SIDE


def test_long_perp_grid_is_bilateral_with_target() -> None:
    orders = grid_orders(
        product="perp",
        perp_mode="long",
        lower_price=Decimal("90"),
        upper_price=Decimal("110"),
        total_grid_count=4,
    )
    assert len(orders.bid_prices) == 2
    assert len(orders.ask_prices) == 2
    assert compute_perp_target("long", len(orders.ask_prices), len(orders.bid_prices), orders.bid_sizes[0]) == Decimal("0.02")


def test_short_perp_grid_target_is_negative() -> None:
    orders = grid_orders(
        product="perp",
        perp_mode="short",
        lower_price=Decimal("90"),
        upper_price=Decimal("110"),
        total_grid_count=4,
    )
    assert compute_perp_target("short", len(orders.ask_prices), len(orders.bid_prices), orders.bid_sizes[0]) == Decimal("-0.02")


def test_shared_perp_contract_table() -> None:
    cases = [
        ("long", Decimal("0.02")),
        ("short", Decimal("-0.02")),
        ("neutral", Decimal(0)),
    ]
    for mode, expected_target in cases:
        orders = grid_orders(
            product="perp",
            perp_mode=mode,
            lower_price=Decimal("90"),
            upper_price=Decimal("110"),
            total_grid_count=4,
        )
        assert len(orders.bid_prices) == 2
        assert len(orders.ask_prices) == 2
        target = compute_perp_target(mode, len(orders.ask_prices), len(orders.bid_prices), orders.bid_sizes[0])
        assert target == expected_target


def test_perp_position_is_safe_requires_convergence_at_zero() -> None:
    cfg = config(
        product="perp",
        perp_mode="long",
        lower_price=Decimal("90"),
        upper_price=Decimal("110"),
        total_grid_count=4,
        max_position=Decimal("0.04"),
    )
    orders = grid_orders(
        product="perp",
        perp_mode="long",
        lower_price=Decimal("90"),
        upper_price=Decimal("110"),
        total_grid_count=4,
        max_position=Decimal("0.04"),
    )
    assert not perp_position_is_safe(cfg, Decimal(0), orders)
    assert perp_position_is_safe(cfg, Decimal("0.02"), orders)


def test_out_of_range_pause_returns_empty_orders() -> None:
    orders = build_perp_orders(
        config(
            product="perp",
            lower_price=Decimal("90"),
            upper_price=Decimal("110"),
            total_grid_count=4,
        ),
        Decimal("120"),
        Decimal("1"),
        Decimal("0.01"),
        Decimal("0.01"),
        Decimal(0),
    )
    assert orders.bid_prices == []
    assert orders.ask_prices == []


@pytest.mark.parametrize(
    ("action", "skip_bulk", "paused", "cancel", "close", "effective"),
    [
        ("pause", True, True, False, False, Decimal("120")),
        ("cancel_orders", True, False, True, False, Decimal("120")),
        ("close_position", True, False, True, True, Decimal("120")),
        ("clamp_continue", False, False, False, False, Decimal("110")),
    ],
)
def test_perp_out_of_range_action_contract(
    action: str,
    skip_bulk: bool,
    paused: bool,
    cancel: bool,
    close: bool,
    effective: Decimal,
) -> None:
    decision = perp_out_of_range_decision(
        config(
            product="perp",
            out_of_range_action=action,
            lower_price=Decimal("90"),
            upper_price=Decimal("110"),
            total_grid_count=4,
        ),
        Decimal("120"),
    )
    assert decision.direction == "above"
    assert decision.skip_bulk is skip_bulk
    assert decision.paused is paused
    assert decision.cancel_orders is cancel
    assert decision.close_position is close
    assert decision.effective_planning_price == effective


def test_perp_risk_trim_keeps_nearest_pending_levels() -> None:
    cfg = config(
        product="perp",
        perp_mode="neutral",
        max_position=Decimal("0.02"),
    )
    orders = GridOrders(
        [Decimal("99"), Decimal("98"), Decimal("97")],
        [Decimal("0.01")] * 3,
        [Decimal("101"), Decimal("102"), Decimal("103")],
        [Decimal("0.01")] * 3,
    )
    trimmed = trim_perp_pending_to_risk(cfg, Decimal(0), orders)
    assert trimmed.bid_prices == [Decimal("99"), Decimal("98")]
    assert trimmed.ask_prices == [Decimal("101"), Decimal("102")]


def test_profit_preview_subtracts_maker_fees_for_both_fills() -> None:
    orders = grid_orders(order_size=Decimal("1"))
    preview = GridMath.profit_preview(orders, Decimal("0.001"))
    assert preview.pair_count == 2
    assert preview.gross_profit == Decimal("30")
    assert preview.maker_fees == Decimal("0.4")
    assert preview.net_profit == Decimal("29.6")


def test_spot_funding_requirement_includes_bid_reservation_and_maker_fee() -> None:
    quote, base, margin = GridMath.funding_requirements(
        grid_orders(order_size=Decimal("1")), Decimal("0.001"), "spot", Decimal("1")
    )
    assert quote == Decimal("185.185")
    assert base == Decimal("2")
    assert margin is None


def test_perp_margin_uses_larger_side_notional_and_preview_leverage() -> None:
    _, _, margin = GridMath.funding_requirements(
        grid_orders(order_size=Decimal("1")), Decimal("0.001"), "perp", Decimal("10")
    )
    assert margin == Decimal("21.9")


def test_directional_grid_has_no_completed_cycle_profit_estimate() -> None:
    preview = GridMath.profit_preview(
        grid_orders(
            product="perp",
            perp_mode="long",
            lower_price=Decimal("90"),
            upper_price=Decimal("110"),
            total_grid_count=4,
        ),
        Decimal("0.001"),
    )
    assert preview.pair_count == 2
    assert preview.net_profit > 0


def test_grid_rejects_mid_outside_range() -> None:
    with pytest.raises(ValueError, match="outside the configured grid range"):
        GridMath.orders(config(), Decimal("110"), Decimal("1"), Decimal("0.01"), Decimal("0.01"))


def test_grid_rejects_order_size_that_rounds_to_zero() -> None:
    with pytest.raises(ValueError, match="rounds to zero"):
        grid_orders(order_size=Decimal("0.001"))


def test_grid_rejects_size_below_market_minimum() -> None:
    with pytest.raises(ValueError, match="below the market minimum"):
        GridMath.orders(
            config(order_size=Decimal("0.01")),
            Decimal("100"),
            Decimal("1"),
            Decimal("0.01"),
            Decimal("0.02"),
        )


def test_spot_funds_available_base_and_quote_never_go_negative() -> None:
    funds = SpotFunds(base_balance=Decimal("-1"), quote_balance=Decimal("-10"))
    assert funds.available_base == Decimal(0)
    assert funds.available_quote == Decimal(0)


def test_spot_funds_available_for_bulk_credits_existing_escrow() -> None:
    funds = SpotFunds(
        base_balance=Decimal("8.078783"),
        quote_balance=Decimal("0.0005"),
        base_reserved=Decimal("70"),
        quote_reserved=Decimal("100"),
        quote_cross_balance=Decimal("958.884555"),
    )
    assert funds.available_base == Decimal("8.078783")
    # Cross USDC is diagnostic only; it must never inflate available_quote.
    assert funds.available_quote == Decimal("0.0005")
    assert funds.available_base_for_bulk == Decimal("78.078783")
    assert funds.available_quote_for_bulk == Decimal("100.0005")


def test_fit_spot_orders_to_funds_keeps_nearest_levels_and_preserves_order() -> None:
    orders = grid_orders(levels_per_side=3, order_size=Decimal("1"))
    assert orders.bid_prices == [Decimal("96"), Decimal("93"), Decimal("90")]
    assert orders.ask_prices == [Decimal("103"), Decimal("106"), Decimal("110")]
    # Only enough quote for 1 bid (96) and enough base for 2 asks (1 + 1).
    funds = SpotFunds(base_balance=Decimal("2"), quote_balance=Decimal("100"))
    fitted = fit_spot_orders_to_funds(orders, funds)
    assert fitted.bid_prices == [Decimal("96")]
    assert fitted.ask_prices == [Decimal("103"), Decimal("106")]
    # Bids stay descending and asks stay ascending, matching the Move bulk ABI requirement.
    assert fitted.bid_prices == sorted(fitted.bid_prices, reverse=True)
    assert fitted.ask_prices == sorted(fitted.ask_prices)


def test_compute_spot_taker_funding_never_spends_bid_reserve() -> None:
    orders = grid_orders(order_size=Decimal("1"))
    # 500 quote free, 400 of it reserved for the grid's own bids -> 100 spare for funding.
    funds = SpotFunds(base_balance=Decimal(0), quote_balance=Decimal("500"))
    orders_with_costly_bids = GridOrders(
        orders.bid_prices,
        [Decimal("4")] * len(orders.bid_prices),
        orders.ask_prices,
        orders.ask_sizes,
    )
    base_gap, limit_price, quantity = compute_spot_taker_funding(
        funds,
        orders_with_costly_bids,
        Decimal("100"),
        Decimal("1"),
        Decimal("0.01"),
        Decimal("0.01"),
    )
    assert base_gap == sum(orders.ask_sizes, Decimal(0))
    cost = quantity * limit_price * (Decimal(1) + TAKER_FEE_BUFFER)
    assert cost <= Decimal("100")


def test_compute_spot_taker_funding_credits_existing_bulk_escrow() -> None:
    orders = grid_orders(order_size=Decimal("1"))
    funds = SpotFunds(
        base_balance=Decimal("1"),
        quote_balance=Decimal("1000"),
        base_reserved=Decimal("2"),
    )
    base_gap, _limit_price, _quantity = compute_spot_taker_funding(
        funds, orders, Decimal("100"), Decimal("1"), Decimal("0.01"), Decimal("0.01")
    )
    # available_base_for_bulk is 3 (1 free + 2 escrowed), which already covers the 2-unit ask
    # requirement, so crediting the escrow must zero the gap rather than leaving it at 1.
    assert base_gap == Decimal(0)
