#[cfg(test)]
mod tests {
    use crate::*;use crate::strategy::perp::risk::{apply_perp_risk_trim, perp_position_is_safe, compute_perp_target};
    use rust_decimal_macros::dec;

    #[test]
    fn bulk_levels_are_visible_to_reconciliation() {
        let row = serde_json::json!({
            "sequence_number": 427,
            "bid_prices": [0.55],
            "bid_sizes": [10.0],
            "ask_prices": [0.56, 0.57],
            "ask_sizes": [10.0, 5.0]
        });
        let mut orders = Vec::new();
        append_bulk_levels(&mut orders, &row, "bid_prices", "bid_sizes", Side::Bid, 427)
            .expect("bulk bids");
        append_bulk_levels(&mut orders, &row, "ask_prices", "ask_sizes", Side::Ask, 427)
            .expect("bulk asks");
        assert_eq!(orders.len(), 3);
        assert_eq!(orders[0].order_id, "bulk:427:Bid:0");
        assert_eq!(orders[1].side, Side::Ask);
        assert_eq!(orders[2].remaining_size, dec!(5));
    }

    #[test]
    fn bulk_levels_reject_mismatched_price_and_size_arrays() {
        let row = serde_json::json!({
            "bid_prices": [0.55],
            "bid_sizes": []
        });
        let error = append_bulk_levels(
            &mut Vec::new(),
            &row,
            "bid_prices",
            "bid_sizes",
            Side::Bid,
            1,
        )
        .expect_err("mismatched bulk arrays must fail");
        assert!(error.to_string().contains("length mismatch"));
    }

    #[test]
    fn resumed_trade_history_includes_both_timestamp_bounds() {
        let start = DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        let end = DateTime::from_timestamp_millis(1_700_000_100_000).unwrap();
        let mut params = Vec::new();

        append_trade_history_window(&mut params, Some(start), end);

        assert_eq!(
            params,
            vec![
                ("start_timestamp", "1699999999999".to_owned()),
                ("end_timestamp", "1700000100000".to_owned()),
            ]
        );
    }

    #[test]
    fn active_bulk_ladder_requires_exact_sequence_and_levels() {
        let expected = journal::BulkLadder {
            operation_id: "operation-1".to_owned(),
            product: Product::Perp,
            market_address: "0x01".to_owned(),
            sequence: 42,
            prior_sequence: Some(41),
            levels: vec![journal::BulkLevelState {
                side: Side::Bid,
                index: 0,
                price: dec!(100),
                original_size: dec!(0.01),
                filled_size: Decimal::ZERO,
            }],
            intent_at: Utc::now(),
            transaction_hash: None,
            cancel_transaction_hash: None,
            state: journal::BulkLadderState::IntentRecorded,
        };
        let mut observed = ActiveBulkLadder {
            product: Product::Perp,
            market_address: "0x1".to_owned(),
            sequence: 42,
            levels: expected.levels.clone(),
        };

        assert!(observed.matches(&expected));
        observed.sequence = 43;
        assert!(!observed.matches(&expected));
        observed.sequence = 42;
        observed.levels[0].original_size = dec!(0.02);
        assert!(!observed.matches(&expected));
    }

    fn market() -> Market {
        Market {
            address: "0x1".to_owned(),
            name: "BTC/USD".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 0,
            sz_decimals: 2,
            product: Product::Perp,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: None,
            quote_symbol: None,
        }
    }
    fn config() -> GridConfig {
        GridConfig {
            product: Product::Perp,
            perp_mode: PerpMode::Neutral,
            market_name: "BTC/USD".to_owned(),
            range: RangeSpec::Percent { percent: dec!(10) },
            total_count: 40,
            allocation: Allocation::TotalBudget(dec!(1000)),
            maker_fee_rate: dec!(0.0001),
            preview_leverage: dec!(1),
            refresh: Duration::from_secs(3),
            price_source: PriceSource::Prices,
            spot: SpotExecutionConfig::default(),
            max_position: None,
            out_of_range_action: OutOfRangeAction::default(),
        }
    }

    #[test]
    fn spot_funds_keep_positions_net_of_in_flight_reservations() {
        // `base_balance`/`quote_balance` come from `spot.positions`, which the account overview's
        // own arithmetic (`total_usd` = positions + reserved) proves is already net of
        // `in_flight_orders`. Available must equal the position balance as-is, not balance minus
        // reserved again — see `available_base`/`available_quote` doc comments for the proof.
        let funds = SpotFunds {
            base_symbol: "BTC".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(0.5),
            quote_balance: dec!(1000),
            base_reserved: dec!(0.2),
            quote_reserved: dec!(400),
            quote_cross_balance: Decimal::ZERO,
        };
        assert_eq!(funds.available_base(), dec!(0.5));
        assert_eq!(funds.available_quote(), dec!(1000));
    }

    #[test]
    fn parse_spot_funds_reads_generic_assets_and_reservations() {
        let overview = serde_json::json!({
            "spot": {
                "positions": [
                    {"asset_symbol": "BTC", "amount": 0.5},
                    {"asset_symbol": "USDC", "amount": 1000.0}
                ],
                "in_flight_orders": [
                    {"reserved_asset": "BTC", "reserved_amount": 0.2},
                    {"reserved_asset": "USDC", "reserved_amount": 400.0}
                ]
            }
        });
        let market = Market {
            address: "0x1".to_owned(),
            name: "BTC/USDC".to_owned(),
            tick_size: dec!(0.01),
            lot_size: dec!(0.00001),
            min_size: dec!(0.00001),
            px_decimals: 2,
            sz_decimals: 5,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BTC".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let funds = parse_spot_funds(&overview, &market).expect("spot funds");
        assert_eq!(funds.available_base(), dec!(0.5));
        assert_eq!(funds.available_quote(), dec!(1000));
    }

    #[test]
    fn spot_funds_never_report_negative_available_balances() {
        // Even though reservations are not subtracted from `positions` again, a negative
        // position balance (which should never happen, but must not panic or underflow) is
        // still floored at zero rather than propagated.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(-1),
            quote_balance: dec!(-10),
            base_reserved: dec!(2),
            quote_reserved: dec!(20),
            quote_cross_balance: Decimal::ZERO,
        };
        assert_eq!(funds.available_base(), Decimal::ZERO);
        assert_eq!(funds.available_quote(), Decimal::ZERO);
    }

    #[test]
    fn in_flight_reservation_is_classified_by_asset_address_from_positions() {
        // The /markets row for APT/USDC returns null asset addresses, while in_flight_orders
        // identifies the reserved asset by address. The base reservation must not be counted
        // against the quote balance.
        let overview = serde_json::json!({
            "usdc_cross_withdrawable_balance": 958.884555,
            "spot": {
                "positions": [
                    {"asset_addr": "0xa", "asset_symbol": "APT", "amount": 8.078783},
                    {"asset_addr": "0x5428", "asset_symbol": "USDC", "amount": 0.0005}
                ],
                "in_flight_orders": [
                    {"reserved_asset": "0xa", "reserved_amount": 70.0}
                ]
            }
        });
        let market = Market {
            address: "0x26f1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(10),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: None,
            quote_symbol: None,
        };
        // Verified against a live testnet account: 8.078783 APT free + 70 APT reserved in a
        // resting sell ladder were both real (78.078783 total, matching the wallet display).
        // `base_reserved` is recorded for display/future verification; `available_base` must
        // still report the free 8.078783, not double-subtract the 70 already excluded from
        // `positions.amount`.
        let funds = parse_spot_funds(&overview, &market).expect("spot funds");
        assert_eq!(funds.base_reserved, dec!(70));
        assert_eq!(funds.quote_reserved, Decimal::ZERO);
        assert_eq!(funds.available_base(), dec!(8.078783));
        // On replacement the resting ladder's escrow is credited by the Move entry function.
        assert_eq!(funds.available_base_for_bulk(), dec!(78.078783));
        // Cross USDC is NOT spendable by `place_bulk_order_from_pfs`; only PFS quote counts.
        assert_eq!(funds.available_quote(), dec!(0.0005));
        assert_eq!(funds.available_quote_for_bulk(), dec!(0.0005));
        assert_eq!(funds.quote_cross_balance(), dec!(958.884555));
    }

    #[test]
    fn spot_funds_do_not_treat_cross_quote_as_bulk_funding() {
        // Verified on-chain: `source_bulk_funds_from_pfs` asserts against
        // `primary_fungible_store::balance` alone, so counting Cross here produced a plan the
        // chain rejected with EINSUFFICIENT_PFS_FUNDS(0x1).
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(8),
            quote_balance: dec!(0.0005),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: dec!(958.884555),
        };
        assert_eq!(funds.available_quote(), dec!(0.0005));
        assert_eq!(funds.quote_cross_balance(), dec!(958.884555));

        // Existing escrow is credited for replacement sizing; Cross funds still are not.
        let with_escrow = SpotFunds {
            quote_reserved: dec!(100),
            ..funds
        };
        assert_eq!(with_escrow.available_quote_for_bulk(), dec!(100.0005));
    }

    #[test]
    fn total_count_splits_between_sides() {
        // With mid at 50 in a [0, 100] range, allocation is 50/50.
        let lower = dec!(0);
        let upper = dec!(100);
        let mid = dec!(50);
        let (bid, ask, _, _) = side_counts(&config(), lower, upper, mid);
        assert_eq!((bid, ask), (20, 20));
        // With mid at 25 in [0, 100], there are only ten bid levels below the mid and thirty
        // ask levels above it. The side and combined limits must both be preserved.
        let mid = dec!(25);
        let (bid, ask, _, _) = side_counts(&config(), lower, upper, mid);
        assert_eq!((bid, ask), (10, 30));
        assert_eq!(bid + ask, MAX_TOTAL_LEVELS);
        assert!(bid <= MAX_LEVELS_PER_SIDE && ask <= MAX_LEVELS_PER_SIDE);
        assert!(
            GridConfig {
                total_count: MAX_TOTAL_LEVELS + 1,
                ..config()
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn plan_obeys_budget_and_level_limit() {
        let mut market = market();
        market.tick_size = dec!(0.1);
        let plan = build_plan(&config(), &market, dec!(100)).unwrap();
        assert_eq!(plan.bids.len(), 20);
        assert_eq!(plan.asks.len(), 20);
        assert!(plan.estimated_margin.unwrap() <= dec!(1000));
    }
    #[test]
    fn step_grid_is_compounded() {
        let plan = build_plan(
            &GridConfig {
                product: Product::Spot,
                range: RangeSpec::StepPercent { percent: dec!(1) },
                total_count: 4,
                allocation: Allocation::FixedSize(dec!(1)),
                ..config()
            },
            &market(),
            dec!(100),
        )
        .unwrap();
        assert_eq!(plan.bids[0].price, dec!(99));
        assert_eq!(plan.asks[0].price, dec!(101));
    }
    #[test]
    fn trade_marks_matching_level_filled() {
        let mut plan = build_plan(
            &GridConfig {
                total_count: 4,
                allocation: Allocation::FixedSize(dec!(1)),
                ..config()
            },
            &market(),
            dec!(100),
        )
        .unwrap();
        let price = plan.bids[0].price;
        plan.apply_trade_history(
            &[Trade {
                price,
                size: dec!(1),
                timestamp_ms: 0,
            }],
            dec!(1),
        );
        assert_eq!(plan.bids[0].state, LevelState::Filled);
    }

    #[test]
    fn documented_perp_trade_fields_book_fee_pnl_and_funding() {
        let row = serde_json::json!({
            "asset_type": "perp",
            "trade_id": "123",
            "order_id": "456",
            "action": "Net",
            "price": "105.5",
            "size": "2",
            "fee_amount": "0.25",
            "realized_pnl_amount": "3.5",
            "realized_funding_amount": "-0.1",
            "transaction_unix_ms": 1_700_000_000_000i64
        });
        let order_sides = std::collections::HashMap::from([("456".to_owned(), false)]);
        let fill = parse_perp_fill(&row, &order_sides).unwrap();
        assert_eq!(fill.id, "123");
        assert_eq!(fill.side, strategy::perp::accounting::FillSide::Sell);
        assert_eq!(fill.fee_quote, dec!(0.25));
        assert_eq!(fill.realized_pnl_quote, dec!(3.5));
        assert_eq!(fill.realized_funding_quote, dec!(-0.1));
    }

    /// A pinned Spot ladder: bounds [90, 110] with eight fixed levels, sized 1 each.
    fn pinned_spot_plan() -> GridPlan {
        let level = |price: Decimal, side: Side| GridLevel {
            side,
            price,
            size: Decimal::ONE,
            notional: price,
            state: LevelState::Planned,
        };
        let bids = vec![
            level(dec!(98), Side::Bid),
            level(dec!(96), Side::Bid),
            level(dec!(94), Side::Bid),
            level(dec!(92), Side::Bid),
        ];
        let asks = vec![
            level(dec!(102), Side::Ask),
            level(dec!(104), Side::Ask),
            level(dec!(106), Side::Ask),
            level(dec!(108), Side::Ask),
        ];
        let quote_required: Decimal =
            bids.iter().map(|l| l.notional).sum::<Decimal>() * dec!(1.001);
        let base_required = asks.iter().map(|l| l.size).sum();
        GridPlan {
            mid: dec!(100),
            lower: dec!(90),
            upper: dec!(110),
            per_grid_base_size: Some(Decimal::ONE),
            bids,
            asks,
            quote_required,
            base_required,
            estimated_margin: None,
            ..Default::default()
        }
    }

    #[test]
    fn projection_never_moves_the_pinned_prices_or_bounds() {
        let pinned = pinned_spot_plan();
        let original_prices: Vec<Decimal> = {
            let mut p: Vec<Decimal> = pinned.all_levels().map(|l| l.price).collect();
            p.sort();
            p
        };

        for mid in [dec!(93), dec!(100), dec!(107)] {
            let projected = pinned.project_spot(mid, dec!(1)).unwrap();
            assert_eq!(projected.lower, pinned.lower, "lower bound moved at {mid}");
            assert_eq!(projected.upper, pinned.upper, "upper bound moved at {mid}");
            let mut prices: Vec<Decimal> = projected.all_levels().map(|l| l.price).collect();
            prices.sort();
            assert_eq!(prices, original_prices, "ladder prices moved at mid {mid}");
            assert!(
                projected.all_levels().all(|l| l.size == Decimal::ONE),
                "per-level size changed at mid {mid}"
            );
        }
    }

    #[test]
    fn falling_price_rotates_former_bids_into_asks() {
        // Price fell from 100 to 95: the 96 and 98 levels were bought on the way down, so the
        // grid must now offer them for sale higher, while only 92/94 remain as buys.
        let projected = pinned_spot_plan().project_spot(dec!(95), dec!(1)).unwrap();

        let bid_prices: Vec<Decimal> = projected.bids.iter().map(|l| l.price).collect();
        let ask_prices: Vec<Decimal> = projected.asks.iter().map(|l| l.price).collect();

        assert_eq!(bid_prices, vec![dec!(94), dec!(92)], "bids must descend");
        assert_eq!(
            ask_prices,
            vec![
                dec!(96),
                dec!(98),
                dec!(102),
                dec!(104),
                dec!(106),
                dec!(108)
            ],
            "levels above the new price must all be asks"
        );
        assert!(projected.bids.iter().all(|l| l.side == Side::Bid));
        assert!(projected.asks.iter().all(|l| l.side == Side::Ask));
    }

    #[test]
    fn rising_price_rotates_former_asks_into_bids() {
        // The mirror case: price rose to 105, so 102/104 were sold and become buy-backs.
        let projected = pinned_spot_plan().project_spot(dec!(105), dec!(1)).unwrap();

        let ask_prices: Vec<Decimal> = projected.asks.iter().map(|l| l.price).collect();
        assert_eq!(ask_prices, vec![dec!(106), dec!(108)]);
        assert_eq!(
            projected.bids.iter().map(|l| l.price).collect::<Vec<_>>(),
            vec![dec!(104), dec!(102), dec!(98), dec!(96), dec!(94), dec!(92)]
        );
    }

    #[test]
    fn a_level_at_the_current_price_is_not_quoted() {
        // Quoting a level at the market price would cross the book or self-trade.
        let projected = pinned_spot_plan().project_spot(dec!(96), dec!(1)).unwrap();
        assert!(
            projected.all_levels().all(|l| l.price != dec!(96)),
            "the level at the current price must be withheld"
        );
        assert_eq!(projected.all_levels().count(), 7);
    }

    #[test]
    fn projection_preserves_the_fee_markup_in_the_quote_reserve() {
        let pinned = pinned_spot_plan();
        let projected = pinned.project_spot(dec!(95), dec!(1)).unwrap();

        let bid_notional: Decimal = projected.bids.iter().map(|l| l.notional).sum();
        // 0.1% maker markup is inferred from the pinned plan, not silently dropped.
        assert_eq!(projected.quote_required, bid_notional * dec!(1.001));
        assert_eq!(
            projected.base_required,
            projected.asks.iter().map(|l| l.size).sum::<Decimal>()
        );
    }

    #[test]
    fn projection_rejects_a_non_positive_price() {
        assert!(
            pinned_spot_plan()
                .project_spot(Decimal::ZERO, dec!(1))
                .is_err()
        );
    }

    #[test]
    fn spot_build_plan_clamps_out_of_range_reference_without_moving_bounds() {
        let config = GridConfig {
            product: Product::Spot,
            range: RangeSpec::Bounds {
                lower: dec!(90),
                upper: dec!(110),
            },
            total_count: 4,
            allocation: Allocation::TotalBudget(dec!(1000)),
            ..config()
        };
        let plan = build_plan(&config, &spot_funding_market(), dec!(80)).unwrap();
        assert_eq!(
            plan.mid,
            dec!(80),
            "snapshot must retain the real market price"
        );
        assert_eq!((plan.lower, plan.upper), (dec!(90), dec!(110)));
        assert!(plan.all_levels().all(|level| level.price >= dec!(90)));
        assert!(plan.all_levels().all(|level| level.price <= dec!(110)));
    }

    #[test]
    fn projection_still_works_when_price_leaves_the_pinned_bounds() {
        // Leaving the range must not error: the pinned grid still describes what to do. Below the
        // lower bound every level sits above the market, so the whole ladder is offered for sale
        // (the base was bought on the way down); above the upper bound the mirror holds.
        let pinned = pinned_spot_plan();
        let market = spot_funding_market();

        let below = pinned.project_spot(dec!(80), dec!(1)).unwrap();
        assert_eq!((below.lower, below.upper), (dec!(90), dec!(110)));
        assert!(
            below.bids.is_empty(),
            "nothing is worth buying below the grid"
        );
        assert_eq!(below.asks.len(), 8);
        assert_eq!(below.quote_required, Decimal::ZERO);

        let above = pinned.project_spot(dec!(120), dec!(1)).unwrap();
        assert_eq!((above.lower, above.upper), (dec!(90), dec!(110)));
        assert!(
            above.asks.is_empty(),
            "nothing is left to sell above the grid"
        );
        assert_eq!(above.bids.len(), 8);
        assert_eq!(above.base_required, Decimal::ZERO);
        // Bids must still descend for the bulk ABI even in this degenerate case.
        assert_eq!(above.bids.first().map(|l| l.price), Some(dec!(108)));
        assert_eq!(above.bids.last().map(|l| l.price), Some(dec!(92)));
        let below_bids: Vec<&GridLevel> = below.bids.iter().collect();
        let below_asks: Vec<&GridLevel> = below.asks.iter().collect();
        prepare_bulk_order_parameters(1, &below_bids, &below_asks, &market)
            .expect("below-bound all-ask projection must satisfy bulk ordering");
        let above_bids: Vec<&GridLevel> = above.bids.iter().collect();
        let above_asks: Vec<&GridLevel> = above.asks.iter().collect();
        prepare_bulk_order_parameters(1, &above_bids, &above_asks, &market)
            .expect("above-bound all-bid projection must satisfy bulk ordering");
    }

    #[test]
    fn executable_clears_trade_history_markers_without_moving_geometry() {
        let mut plan = pinned_spot_plan();
        plan.bids[0].state = LevelState::Filled;
        let executable = plan.executable();
        assert!(
            executable
                .all_levels()
                .all(|level| level.state == LevelState::Planned)
        );
        assert_eq!(executable.lower, plan.lower);
        assert_eq!(executable.upper, plan.upper);
        assert_eq!(
            executable
                .all_levels()
                .map(|level| level.price)
                .collect::<Vec<_>>(),
            plan.all_levels()
                .map(|level| level.price)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rebuilt_execution_plan_ignores_historical_fill_markers() {
        let config = GridConfig {
            total_count: 4,
            allocation: Allocation::FixedSize(dec!(1)),
            ..config()
        };
        let market = market();
        let mut display_plan = build_plan(&config, &market, dec!(100)).unwrap();
        let filled_price = display_plan.bids[0].price;
        display_plan.apply_trade_history(
            &[Trade {
                price: filled_price,
                size: dec!(1),
                timestamp_ms: 0,
            }],
            market.tick_size,
        );
        assert_eq!(display_plan.bids[0].state, LevelState::Filled);

        // Live reconciliation rebuilds the plan from market/config rather than trusting an old
        // trade-history marker, so the level remains eligible for a future order.
        let executable_plan = build_plan(&config, &market, dec!(100)).unwrap();
        assert!(
            executable_plan
                .all_levels()
                .all(|level| level.state == LevelState::Planned)
        );
    }

    #[test]
    fn api_key_format_rejects_empty_whitespace_and_control_values() {
        assert!(validate_api_key_format("").is_err());
        assert!(validate_api_key_format(" key").is_err());
        assert!(validate_api_key_format("key\nvalue").is_err());
        assert!(validate_api_key_format("valid-key").is_ok());
    }

    #[test]
    fn api_key_format_rejects_unreasonably_long_values() {
        let key = "k".repeat(513);
        assert!(validate_api_key_format(&key).is_err());
        assert!(validate_api_key_format(&"k".repeat(512)).is_ok());
    }

    #[test]
    fn recorded_funding_order_match_requires_post_only_buy_price_and_size() {
        let matching = serde_json::json!({
            "is_buy": true,
            "time_in_force": "POST_ONLY",
            "price": 5.995,
            "orig_size": 100.0,
            "remaining_size": 40.0
        });
        assert!(is_recorded_funding_order(&matching, dec!(5.995), dec!(100)));

        let manual_buy = serde_json::json!({
            "is_buy": true,
            "time_in_force": "GTC",
            "price": 5.995,
            "orig_size": 100.0
        });
        assert!(!is_recorded_funding_order(
            &manual_buy,
            dec!(5.995),
            dec!(100)
        ));

        let different_size = serde_json::json!({
            "is_buy": true,
            "time_in_force": "POST_ONLY",
            "price": 5.995,
            "orig_size": 101.0
        });
        assert!(!is_recorded_funding_order(
            &different_size,
            dec!(5.995),
            dec!(100)
        ));
    }

    fn spot_funding_market() -> Market {
        Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        }
    }

    fn spot_funding_funds(base: Decimal, quote: Decimal) -> SpotFunds {
        SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: base,
            quote_balance: quote,
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        }
    }

    fn spot_funding_grid(base_required: Decimal, quote_required: Decimal) -> GridPlan {
        GridPlan {
            mid: dec!(0.5372),
            lower: dec!(0.4834),
            upper: dec!(0.591),
            per_grid_base_size: None,
            bids: vec![],
            asks: vec![],
            quote_required,
            base_required,
            estimated_margin: None,
            ..Default::default()
        }
    }

    #[test]
    fn taker_funding_buys_the_full_gap_when_quote_surplus_allows() {
        // The reported live case: 40 bids reserve ~500 USDC of a ~969 USDC PFS balance, leaving
        // enough surplus to buy the entire missing ask inventory rather than shrinking to 6 asks.
        let funds = spot_funding_funds(dec!(64.367828), dec!(969.539875));
        let grid = spot_funding_grid(dec!(120), dec!(499.830392));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5375), &spot_funding_market())
                .unwrap();
        assert_eq!(funding.base_gap, dec!(55.632172));
        // Whole gap is affordable, so the plan buys it all and the ask side is never shrunk.
        assert_eq!(funding.quantity, dec!(55.63));
    }

    #[test]
    fn taker_funding_never_spends_quote_reserved_for_the_bids() {
        // Only 20 USDC is spare above the bid reserve, so the buy must be bounded by that surplus
        // (including taker-fee headroom), not by the much larger base gap.
        let funds = spot_funding_funds(Decimal::ZERO, dec!(520));
        let grid = spot_funding_grid(dec!(800), dec!(500));
        let market = spot_funding_market();
        let funding = compute_spot_taker_funding(&funds, &grid, dec!(0.5), &market).unwrap();
        assert_eq!(funding.quote_surplus, dec!(20));
        let inclusive_cost =
            funding.quantity * funding.limit_price * (Decimal::ONE + Decimal::new(1, 3));
        assert!(
            inclusive_cost <= funding.quote_surplus,
            "cost {inclusive_cost} must stay within surplus {}",
            funding.quote_surplus
        );
        // Buying is still bounded well below the 800 APT gap.
        assert!(funding.quantity < funding.base_gap);
    }

    #[test]
    fn taker_funding_limit_price_crosses_the_spread_but_is_bounded() {
        let funds = spot_funding_funds(Decimal::ZERO, dec!(1000));
        let grid = spot_funding_grid(dec!(100), dec!(500));
        let market = spot_funding_market();
        let best_ask = dec!(0.5375);
        let funding = compute_spot_taker_funding(&funds, &grid, best_ask, &market).unwrap();
        // Aggressive enough to take the resting ask, but never an unbounded market order.
        assert!(funding.limit_price >= best_ask);
        assert!(funding.limit_price <= best_ask * dec!(1.004));
    }

    #[test]
    fn funding_slice_splits_large_orders_and_respects_minimums() {
        let market = spot_funding_market();
        let slice = spot_funding_slice(dec!(1002.47), &market, 0);
        assert!(slice < dec!(1002.47));
        assert!(slice >= market.min_size);
        assert_eq!(slice, dec!(250.61));
        let backed_off = spot_funding_slice(dec!(1002.47), &market, 2);
        assert!(backed_off < slice);
        assert!(backed_off >= market.min_size);
        assert_eq!(spot_funding_slice(Decimal::ZERO, &market, 0), Decimal::ZERO);
    }

    #[test]
    fn taker_funding_never_spends_the_bid_reserve_on_first_placement() {
        // Reproduces the live incident in /tmp/decibel-spot.log: a 40-level APT/USDC grid needing
        // 988.20 APT of ask inventory and 335.690680 USDC of bid reserve, against an account
        // holding ~533 USDC and essentially no base.
        //
        // The old formula capped the surplus by `available_quote()`, but on a first placement the
        // free balance *contains* the bid reserve — so the IOC was cleared to sweep the entire
        // 533 USDC into base, leaving 3.78 USDC to fund bids that needed 335.69.
        let funds = spot_funding_funds(dec!(0.00696), dec!(533.37));
        let grid = spot_funding_grid(dec!(988.20), dec!(335.690680));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5343), &spot_funding_market())
                .unwrap();

        let spend = funding.quantity * funding.limit_price;
        let reserve = grid.quote_required;
        assert!(
            spend <= funds.available_quote() - reserve,
            "IOC would spend {spend} of {}, encroaching on the {reserve} bid reserve",
            funds.available_quote()
        );
        // The shortfall is real and must remain visible: the caller stops instead of churning.
        assert!(
            funding.base_gap > funding.quantity,
            "the unfundable remainder must still be reported"
        );
    }

    #[test]
    fn taker_funding_reports_no_gap_when_inventory_is_already_sufficient() {
        let funds = spot_funding_funds(dec!(150), dec!(1000));
        let grid = spot_funding_grid(dec!(120), dec!(500));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5375), &spot_funding_market())
                .unwrap();
        assert_eq!(funding.base_gap, Decimal::ZERO);
        assert_eq!(funding.quantity, Decimal::ZERO);
    }

    #[test]
    fn quote_funding_sells_only_base_above_the_ask_reserve() {
        let funds = spot_funding_funds(dec!(200), dec!(100));
        let grid = spot_funding_grid(dec!(120), dec!(500));
        let funding =
            compute_spot_quote_funding(&funds, &grid, dec!(10), &spot_funding_market()).unwrap();
        assert_eq!(funding.quote_gap, dec!(400));
        assert_eq!(funding.base_surplus, dec!(80));
        assert_eq!(funding.limit_price, dec!(9.9700));
        assert_eq!(funding.quantity, dec!(40.16));
    }

    #[test]
    fn quote_funding_does_not_sell_when_base_is_reserved_for_asks() {
        let funds = spot_funding_funds(dec!(120), dec!(100));
        let grid = spot_funding_grid(dec!(120), dec!(500));
        let funding =
            compute_spot_quote_funding(&funds, &grid, dec!(10), &spot_funding_market()).unwrap();
        assert_eq!(funding.base_surplus, Decimal::ZERO);
        assert_eq!(funding.quantity, Decimal::ZERO);
    }

    #[test]
    fn quote_funding_rejects_a_non_positive_bid() {
        let funds = spot_funding_funds(dec!(200), dec!(100));
        let grid = spot_funding_grid(dec!(120), dec!(500));
        assert!(
            compute_spot_quote_funding(&funds, &grid, Decimal::ZERO, &spot_funding_market())
                .is_err()
        );
    }

    #[test]
    fn taker_funding_counts_existing_bulk_escrow_as_held_inventory() {
        // Reproduces the live symptom: an active ladder already escrows 36 APT while only 1.63
        // sits free in PFS. The replacement ABI credits that escrow, so the funding gap is
        // measured against base + escrow. Using the free balance alone would re-buy inventory
        // the account already owns.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(1.63264),
            quote_balance: dec!(488.418878),
            base_reserved: dec!(36),
            quote_reserved: dec!(499.876920),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = spot_funding_grid(dec!(40), dec!(499.876920));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5368), &spot_funding_market())
                .unwrap();
        // 40 needed - (1.63264 free + 36 escrowed) = 2.36736, NOT 40 - 1.63264 = 38.36736.
        assert_eq!(funding.base_gap, dec!(2.36736));
        assert!(
            funding.quantity <= dec!(2.37),
            "must not overbuy past the true gap, got {}",
            funding.quantity
        );
    }

    #[test]
    fn taker_funding_ignores_escrow_it_does_not_have() {
        // Same plan, but with no resting ladder: the whole ask side must be bought.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(1.63264),
            quote_balance: dec!(988.295798),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = spot_funding_grid(dec!(40), dec!(499.876920));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5368), &spot_funding_market())
                .unwrap();
        assert_eq!(funding.base_gap, dec!(38.36736));
    }

    #[test]
    fn taker_funding_sees_spare_quote_while_a_ladder_is_resting() {
        // Live shape: the resting ladder escrows ~499.88 USDC of bids and 36 APT of asks, so free
        // PFS quote (488.42) is BELOW quote_required. Measuring surplus against free PFS alone
        // reports zero spare and refuses to fund, even though the replacement credits the escrow
        // and the free balance is genuinely available to buy base with.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(1.63264),
            quote_balance: dec!(488.418878),
            base_reserved: dec!(36),
            quote_reserved: dec!(499.876920),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = spot_funding_grid(dec!(40), dec!(499.876920));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5368), &spot_funding_market())
                .unwrap();
        assert!(
            funding.quote_surplus > Decimal::ZERO,
            "escrowed bids must not mask the spare free balance"
        );
        assert_eq!(funding.base_gap, dec!(2.36736));
        assert!(
            funding.quantity >= market_min(),
            "must actually fund the gap"
        );
    }

    #[test]
    fn taker_funding_never_promises_more_quote_than_pfs_holds() {
        // Escrow makes `available_quote_for_bulk` large, but an IOC can only spend free PFS.
        // The surplus must stay within the free balance or the IOC aborts on-chain.
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: Decimal::ZERO,
            quote_balance: dec!(10),
            base_reserved: Decimal::ZERO,
            quote_reserved: dec!(900),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = spot_funding_grid(dec!(100), dec!(500));
        let funding =
            compute_spot_taker_funding(&funds, &grid, dec!(0.5), &spot_funding_market()).unwrap();
        assert!(
            funding.quote_surplus <= funds.available_quote(),
            "surplus {} exceeded free PFS {}",
            funding.quote_surplus,
            funds.available_quote()
        );
    }

    fn market_min() -> Decimal {
        spot_funding_market().min_size
    }

    #[test]
    fn taker_funding_rejects_a_non_positive_ask() {
        let funds = spot_funding_funds(Decimal::ZERO, dec!(1000));
        let grid = spot_funding_grid(dec!(100), dec!(500));
        assert!(
            compute_spot_taker_funding(&funds, &grid, Decimal::ZERO, &spot_funding_market())
                .is_err()
        );
    }

    #[test]
    fn compute_spot_funding_plan_when_base_is_sufficient_does_not_buy() {
        let funds = SpotFunds {
            base_symbol: "BTC".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(1.5),
            quote_balance: dec!(2000),
            base_reserved: dec!(0.2),
            quote_reserved: dec!(500),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(60000),
            lower: dec!(57000),
            upper: dec!(63000),
            per_grid_base_size: None,
            bids: vec![],
            asks: vec![],
            quote_required: dec!(1000),
            base_required: dec!(1),
            estimated_margin: None,
            ..Default::default()
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "BTC/USDC".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.001),
            min_size: dec!(0.001),
            px_decimals: 0,
            sz_decimals: 3,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BTC".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan =
            compute_spot_funding_plan(&funds, &grid, dec!(59900), dec!(60000), &market).unwrap();
        assert_eq!(plan.base_gap, Decimal::ZERO);
        assert_eq!(plan.buy_quantity, Decimal::ZERO);
        assert!(plan.buy_price.is_none());
    }

    #[test]
    fn compute_spot_funding_plan_can_use_mid_when_order_book_has_no_bid() {
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: Decimal::ZERO,
            quote_balance: dec!(1000),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(0.5782),
            lower: dec!(0.55),
            upper: dec!(0.61),
            per_grid_base_size: None,
            bids: vec![],
            asks: vec![],
            quote_required: dec!(400),
            base_required: dec!(100),
            estimated_margin: None,
            ..Default::default()
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        // The caller passes mid as the best-bid fallback when depth is temporarily empty.
        let plan = compute_spot_funding_plan(&funds, &grid, grid.mid, grid.mid, &market).unwrap();
        assert_eq!(plan.buy_price, Some(dec!(0.5779)));
        assert_eq!(plan.buy_quantity, dec!(100));
    }

    #[test]
    fn compute_spot_funding_plan_when_quote_gap_exists_does_not_buy() {
        let funds = SpotFunds {
            base_symbol: "BTC".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(0.5),
            quote_balance: dec!(200),
            base_reserved: dec!(0),
            quote_reserved: dec!(0),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(60000),
            lower: dec!(57000),
            upper: dec!(63000),
            per_grid_base_size: None,
            bids: vec![],
            asks: vec![],
            quote_required: dec!(500),
            base_required: dec!(1),
            estimated_margin: None,
            ..Default::default()
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "BTC/USDC".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.001),
            min_size: dec!(0.001),
            px_decimals: 0,
            sz_decimals: 3,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BTC".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan =
            compute_spot_funding_plan(&funds, &grid, dec!(59900), dec!(60000), &market).unwrap();
        assert_eq!(plan.quote_gap, dec!(300));
        assert_eq!(plan.buy_quantity, Decimal::ZERO);
        assert!(plan.buy_price.is_none());
    }

    #[test]
    fn funding_plan_rounds_up_one_lot_within_one_percent_grid_tolerance() {
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: Decimal::ZERO,
            quote_balance: dec!(100),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(1),
            lower: dec!(0.9),
            upper: dec!(1.1),
            per_grid_base_size: None,
            bids: vec![],
            asks: vec![],
            quote_required: dec!(99.995),
            base_required: dec!(0.011),
            estimated_margin: None,
            ..Default::default()
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan = compute_spot_funding_plan(&funds, &grid, dec!(1), dec!(1), &market).unwrap();
        assert_eq!(plan.buy_quantity, dec!(0.02));
        assert!(plan.buy_quantity >= grid.base_required);
        assert!(plan.borrowed_from_grid_quote > Decimal::ZERO);
        assert!(plan.borrowed_from_grid_quote <= grid.quote_required * dec!(0.01));
    }

    #[test]
    fn funding_plan_refuses_round_up_beyond_one_percent_grid_tolerance() {
        let funds = SpotFunds {
            base_symbol: "APT".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: Decimal::ZERO,
            quote_balance: dec!(100),
            base_reserved: Decimal::ZERO,
            quote_reserved: Decimal::ZERO,
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(1),
            lower: dec!(0.9),
            upper: dec!(1.1),
            per_grid_base_size: None,
            bids: vec![],
            asks: vec![],
            quote_required: dec!(99.995),
            base_required: dec!(2),
            estimated_margin: None,
            ..Default::default()
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.0001),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 4,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan = compute_spot_funding_plan(&funds, &grid, dec!(1), dec!(1), &market).unwrap();
        assert!(plan.buy_quantity < grid.base_required);
        assert_eq!(plan.borrowed_from_grid_quote, Decimal::ZERO);
    }

    #[test]
    fn compute_spot_funding_plan_when_quote_spare_after_grid_is_used_to_buy_base() {
        let funds = SpotFunds {
            base_symbol: "BTC".to_owned(),
            quote_symbol: "USDC".to_owned(),
            base_balance: dec!(0.5),
            quote_balance: dec!(2000),
            base_reserved: dec!(0),
            quote_reserved: dec!(0),
            quote_cross_balance: Decimal::ZERO,
        };
        let grid = GridPlan {
            mid: dec!(60000),
            lower: dec!(57000),
            upper: dec!(63000),
            per_grid_base_size: None,
            bids: vec![],
            asks: vec![],
            quote_required: dec!(500),
            base_required: dec!(1),
            estimated_margin: None,
            ..Default::default()
        };
        let market = Market {
            address: "0x1".to_owned(),
            name: "BTC/USDC".to_owned(),
            tick_size: dec!(1),
            lot_size: dec!(0.001),
            min_size: dec!(0.001),
            px_decimals: 0,
            sz_decimals: 3,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("BTC".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan =
            compute_spot_funding_plan(&funds, &grid, dec!(59900), dec!(60000), &market).unwrap();
        assert!(plan.base_gap > Decimal::ZERO);
        assert!(plan.buy_price.is_some());
        assert!(plan.buy_quantity > Decimal::ZERO);
        let buy_price = plan.buy_price.unwrap();
        assert!(buy_price < dec!(59900), "buy price must be below best bid");
        assert!(
            buy_price < dec!(60000),
            "buy price must be below market mid"
        );
        let capped = dec!(59900).min(dec!(60000)) * dec!(9995) / dec!(10000);
        let expected_price = (capped / market.tick_size).floor() * market.tick_size;
        assert_eq!(buy_price, expected_price);
        let available_base = funds.available_base();
        let base_gap = (grid.base_required - available_base).max(Decimal::ZERO);
        let spare_q = ((funds.available_quote() - grid.quote_required).max(Decimal::ZERO)
            / (Decimal::ONE + Decimal::new(1, 3)))
        .floor();
        let raw = base_gap.min(spare_q / buy_price);
        let expected_qty = (raw / market.lot_size).floor() * market.lot_size;
        assert_eq!(plan.buy_quantity, expected_qty);
    }

    #[test]
    fn spot_plan_rejects_interval_below_fee_and_margin_threshold() {
        let market = spot_funding_market();
        let config = GridConfig {
            product: Product::Spot,
            range: RangeSpec::Bounds {
                lower: dec!(100),
                upper: dec!(100.4),
            },
            total_count: 4,
            allocation: Allocation::FixedSize(dec!(1)),
            maker_fee_rate: dec!(0.0004),
            spot: SpotExecutionConfig {
                min_net_margin_bps: dec!(15),
                ..SpotExecutionConfig::default()
            },
            ..config()
        };
        assert!(build_plan(&config, &market, dec!(100.2)).is_err());
    }

    #[test]
    fn bulk_json_keeps_one_fixed_spot_size_after_recenter() {
        let mut market = market();
        market.product = Product::Spot;
        market.name = "APT/USDC".to_owned();
        let config = GridConfig {
            product: Product::Spot,
            range: RangeSpec::Bounds {
                lower: dec!(90),
                upper: dec!(110),
            },
            total_count: 4,
            allocation: Allocation::TotalBudget(dec!(1)),
            maker_fee_rate: dec!(0.001),
            spot: SpotExecutionConfig {
                total_quote_budget: Some(dec!(4000)),
                total_base_budget: Some(dec!(20)),
                ..SpotExecutionConfig::default()
            },
            ..config()
        };
        let initial = build_plan(&config, &market, dec!(100)).unwrap();
        assert_eq!(initial.per_grid_base_size, Some(dec!(10)));
        let initial_bids = initial.bids.iter().collect::<Vec<_>>();
        let initial_asks = initial.asks.iter().collect::<Vec<_>>();
        let initial_bulk = prepare_bulk_order_parameters(1, &initial_bids, &initial_asks, &market)
            .expect("initial Spot bulk parameters");
        println!(
            "initial bulk order JSON: {}",
            serde_json::to_string(&initial_bulk).expect("serialize initial bulk parameters")
        );
        assert!(
            initial_bulk
                .bid_sizes
                .iter()
                .chain(&initial_bulk.ask_sizes)
                .all(|size| *size == 1000)
        );

        // A fill prompts the live loop to re-project around the newest mid before submitting its
        // replacement. The side counts change from 2/2 to 3/1, but every raw Move size remains
        // the same persisted 10.00 APT value.
        let recentered = initial.project_spot(dec!(106), market.tick_size).unwrap();
        recentered.enforce_spot_budget(&config).unwrap();
        assert_eq!(recentered.per_grid_base_size, initial.per_grid_base_size);
        let recentered_bids = recentered.bids.iter().collect::<Vec<_>>();
        let recentered_asks = recentered.asks.iter().collect::<Vec<_>>();
        let recentered_bulk =
            prepare_bulk_order_parameters(2, &recentered_bids, &recentered_asks, &market)
                .expect("recentered Spot bulk parameters");
        println!(
            "recentered bulk order JSON: {}",
            serde_json::to_string(&recentered_bulk).expect("serialize recentered bulk parameters")
        );
        assert!(
            recentered_bulk
                .bid_sizes
                .iter()
                .chain(&recentered_bulk.ask_sizes)
                .all(|size| *size == 1000)
        );
    }

    #[test]
    fn accepted_partial_entry_resizes_only_ask_inventory() {
        let market = spot_funding_market();
        let plan = GridPlan {
            mid: dec!(100),
            lower: dec!(90),
            upper: dec!(110),
            per_grid_base_size: None,
            bids: vec![GridLevel {
                side: Side::Bid,
                price: dec!(99),
                size: dec!(1),
                notional: dec!(99),
                state: LevelState::Planned,
            }],
            asks: vec![
                GridLevel {
                    side: Side::Ask,
                    price: dec!(101),
                    size: dec!(1),
                    notional: dec!(101),
                    state: LevelState::Planned,
                },
                GridLevel {
                    side: Side::Ask,
                    price: dec!(102),
                    size: dec!(1),
                    notional: dec!(102),
                    state: LevelState::Planned,
                },
            ],
            quote_required: dec!(99),
            base_required: dec!(2),
            estimated_margin: None,
            ..Default::default()
        };
        let resized = plan
            .resize_asks_to_available_base(dec!(1), &market)
            .unwrap();
        assert_eq!(resized.quote_required, plan.quote_required);
        assert!(resized.base_required <= dec!(1));
        assert_eq!(resized.bids[0].size, dec!(1));
    }

    #[test]
    fn perp_long_places_bilateral_grid_with_target() {
        let cfg = GridConfig {
            perp_mode: PerpMode::Long,
            total_count: 4,
            allocation: Allocation::FixedSize(dec!(0.01)),
            range: RangeSpec::Bounds {
                lower: dec!(90),
                upper: dec!(110),
            },
            ..config()
        };
        let plan = build_plan(&cfg, &market(), dec!(100)).unwrap();
        assert!(!plan.bids.is_empty());
        assert!(!plan.asks.is_empty());
        assert_eq!(plan.bids.len() + plan.asks.len(), 4);
        assert_eq!(plan.target_position, Some(dec!(0.02)));
    }

    #[test]
    fn perp_short_target_is_negative_bid_inventory() {
        let cfg = GridConfig {
            perp_mode: PerpMode::Short,
            total_count: 4,
            allocation: Allocation::FixedSize(dec!(0.01)),
            range: RangeSpec::Bounds {
                lower: dec!(90),
                upper: dec!(110),
            },
            ..config()
        };
        let plan = build_plan(&cfg, &market(), dec!(100)).unwrap();
        assert_eq!(plan.target_position, Some(dec!(-0.02)));
    }

    #[test]
    fn perp_neutral_uniform_split_at_midpoint() {
        let lower = dec!(1);
        let upper = dec!(100);
        let mid = dec!(50);
        let plan = build_plan(
            &GridConfig {
                perp_mode: PerpMode::Neutral,
                total_count: 40,
                range: RangeSpec::Bounds { lower, upper },
                ..config()
            },
            &market(),
            mid,
        )
        .unwrap();
        assert_eq!(plan.bids.len(), 20);
        assert_eq!(plan.asks.len(), 20);
        assert_eq!(plan.target_position, Some(Decimal::ZERO));
    }

    #[test]
    fn perp_rejects_more_than_forty_total_levels() {
        let err = GridConfig {
            total_count: 41,
            ..config()
        }
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("40"));
    }

    #[test]
    fn perp_position_is_safe_respects_mode_constraints_at_target() {
        let cfg = GridConfig {
            perp_mode: PerpMode::Long,
            total_count: 4,
            allocation: Allocation::FixedSize(dec!(1)),
            max_position: Some(dec!(4)),
            range: RangeSpec::Bounds {
                lower: dec!(90),
                upper: dec!(110),
            },
            ..config()
        };
        let plan = build_plan(&cfg, &market(), dec!(100)).unwrap();
        let target = plan.target_position.unwrap();
        assert!(perp_position_is_safe(target, &plan, &cfg));
        assert!(!perp_position_is_safe(Decimal::ZERO, &plan, &cfg));
    }

    #[test]
    fn shared_perp_contract_table() {
        let cases = [
            (PerpMode::Long, dec!(100), dec!(0.02)),
            (PerpMode::Short, dec!(100), dec!(-0.02)),
            (PerpMode::Neutral, dec!(100), Decimal::ZERO),
            (PerpMode::Rotate, dec!(100), Decimal::ZERO),
        ];
        for (mode, mid, expected_target) in cases {
            let plan = build_plan(
                &GridConfig {
                    perp_mode: mode,
                    total_count: 4,
                    allocation: Allocation::FixedSize(dec!(0.01)),
                    range: RangeSpec::Bounds {
                        lower: dec!(90),
                        upper: dec!(110),
                    },
                    ..config()
                },
                &market(),
                mid,
            )
            .unwrap();
            assert_eq!(plan.bids.len(), 2);
            assert_eq!(plan.asks.len(), 2);
            assert_eq!(plan.target_position, Some(expected_target));
        }
    }

    #[test]
    fn spot_plan_regression_golden() {
        let config = GridConfig {
            product: Product::Spot,
            perp_mode: PerpMode::Neutral,
            market_name: "APT/USDC".to_owned(),
            range: RangeSpec::Percent { percent: dec!(10) },
            total_count: 8,
            allocation: Allocation::FixedSize(dec!(1)),
            maker_fee_rate: dec!(0.001),
            preview_leverage: dec!(1),
            refresh: Duration::from_secs(3),
            price_source: PriceSource::Prices,
            spot: SpotExecutionConfig::default(),
            max_position: None,
            out_of_range_action: OutOfRangeAction::default(),
        };
        let market = Market {
            address: "0xspot".to_owned(),
            name: "APT/USDC".to_owned(),
            tick_size: dec!(0.01),
            lot_size: dec!(0.01),
            min_size: dec!(0.01),
            px_decimals: 2,
            sz_decimals: 2,
            product: Product::Spot,
            base_asset_addr: None,
            quote_asset_addr: None,
            base_symbol: Some("APT".to_owned()),
            quote_symbol: Some("USDC".to_owned()),
        };
        let plan = build_plan(&config, &market, dec!(10)).unwrap();
        assert_eq!(plan.bids.len(), 4);
        assert_eq!(plan.asks.len(), 4);
        assert_eq!(plan.bids[0].price, dec!(9.75));
        assert_eq!(plan.asks[0].price, dec!(10.25));
        assert!(plan.bids.iter().all(|level| level.size == dec!(1)));
        assert!(plan.asks.iter().all(|level| level.size == dec!(1)));
        assert_eq!(plan.per_grid_base_size, Some(dec!(1)));
    }
}
