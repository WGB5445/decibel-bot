package telegram

import (
	"strings"
	"testing"
	"time"

	"decibel-mm-bot/api"
	"decibel-mm-bot/botstate"
)

func TestPositionsForDisplay_filtersAndSorts(t *testing.T) {
	snap := botstate.Snapshot{
		AllPositions: []botstate.Position{
			{MarketID: "0xbbb", Size: 0.1},
			{MarketID: "0xaaa", Size: 0, IsDeleted: false},
			{MarketID: "0xccc", Size: -0.2, IsDeleted: true},
			{MarketID: "0xAAA", Size: 0.05},
		},
	}
	got := positionsForDisplay(snap)
	if len(got) != 2 {
		t.Fatalf("len=%d want 2 (zero size + deleted filtered)", len(got))
	}
	if got[0].MarketID != "0xAAA" || got[1].MarketID != "0xbbb" {
		t.Fatalf("order wrong: %#v", got)
	}
}

func TestPositionsTotalPages_andClamp(t *testing.T) {
	snap := botstate.Snapshot{
		AllPositions: []botstate.Position{
			{MarketID: "0xa", Size: 1},
			{MarketID: "0xb", Size: 1},
			{MarketID: "0xc", Size: 1},
			{MarketID: "0xd", Size: 1},
		},
	}
	if PositionsTotalPages(snap) != 2 {
		t.Fatalf("total pages=%d want 2", PositionsTotalPages(snap))
	}
	if got := ClampPositionsPage(-1, snap); got != 0 {
		t.Fatalf("Clamp(-1)=%d want 0", got)
	}
	if got := ClampPositionsPage(99, snap); got != 1 {
		t.Fatalf("Clamp(99)=%d want 1", got)
	}
	empty := botstate.Snapshot{}
	if PositionsTotalPages(empty) != 1 {
		t.Fatalf("empty total pages=%d want 1", PositionsTotalPages(empty))
	}
	if ClampPositionsPage(5, empty) != 0 {
		t.Fatalf("empty clamp=%d want 0", ClampPositionsPage(5, empty))
	}
}

func TestFormatPositions_paging(t *testing.T) {
	now := time.Now()
	snap := botstate.Snapshot{
		LastCycleAt:      now,
		TargetMarketID:   "0xtarget",
		TargetMarketName: "BTC/USD",
		AllPositions: []botstate.Position{
			{MarketID: "0x04", Size: 0.01, UserLeverage: 5, EntryPrice: 1},
			{MarketID: "0x02", Size: 0.01, UserLeverage: 5, EntryPrice: 1},
			{MarketID: "0x03", Size: 0.01, UserLeverage: 5, EntryPrice: 1},
			{MarketID: "0x01", Size: 0.01, UserLeverage: 5, EntryPrice: 1},
		},
	}
	name := func(addr string) string {
		return "M-" + addr
	}
	p0 := formatPositions(snap, 0, name)
	if !strings.Contains(p0, "第 1/2 页") {
		t.Fatalf("page header missing: %q", p0)
	}
	firstLine := strings.SplitN(p0, "\n", 2)[0]
	i := strings.Index(firstLine, "*📊")
	j := strings.Index(firstLine, "_第 ")
	if i < 0 || j < 0 || j <= i {
		t.Fatalf("want title then page on same line: %q", firstLine)
	}
	if strings.Count(p0, "M-0x") != 3 {
		t.Fatalf("want 3 markets on page 0, got:\n%s", p0)
	}
	p1 := formatPositions(snap, 1, name)
	if !strings.Contains(p1, "第 2/2 页") {
		t.Fatalf("page 2 header missing: %q", p1)
	}
	first1 := strings.SplitN(p1, "\n", 2)[0]
	i1 := strings.Index(first1, "*📊")
	j1 := strings.Index(first1, "_第 ")
	if i1 < 0 || j1 < 0 || j1 <= i1 {
		t.Fatalf("want title then page on same line: %q", first1)
	}
	if strings.Count(p1, "M-0x") != 1 {
		t.Fatalf("want 1 market on page 1, got:\n%s", p1)
	}
}

func TestFormatPositions_showsPerMarketPnL(t *testing.T) {
	now := time.Now()
	snap := botstate.Snapshot{
		LastCycleAt:      now,
		TargetMarketID:   "0xtarget",
		TargetMarketName: "BTC/USD",
		MidByMarket: map[string]float64{
			"abc": 110,
		},
		AllPositions: []botstate.Position{
			{
				MarketID:                  "0xabc",
				Size:                      2,
				EntryPrice:                100,
				UserLeverage:              3,
				UnrealizedFunding:         -0.25,
				EstimatedLiquidationPrice: 50,
			},
		},
	}
	got := formatPositions(snap, 0, func(string) string { return "ETH/USD" })
	for _, want := range []string{
		"ETH/USD",
		"*多 ▲*",
		"持仓 `2.00000`",
		"开仓价 `$100.00` · 当前价 `$110.00`",
		"持仓价值 `$220.00`",
		"盈利(估算) `+$20.00 (+10.00%)`",
		"资金费 `$-0.2500`",
		"强平 `$50.00`",
	} {
		if !strings.Contains(got, want) {
			t.Fatalf("missing %q in:\n%s", want, got)
		}
	}
}

func TestTradeActionDisplay(t *testing.T) {
	if got, want := tradeActionDisplay("CloseLong"), "平多"; got != want {
		t.Fatalf("CloseLong: got %q want %q", got, want)
	}
	if got, want := tradeActionDisplay("close_short"), "平空"; got != want {
		t.Fatalf("close_short: got %q want %q", got, want)
	}
	if got, want := tradeActionDisplay("OpenLong"), "开多"; got != want {
		t.Fatalf("OpenLong: got %q want %q", got, want)
	}
}

func TestFormatRecentTrades_pagingAndLayout(t *testing.T) {
	var items []api.TradeHistoryItem
	for i := 0; i < 12; i++ {
		items = append(items, api.TradeHistoryItem{
			Market:            "0xm",
			Action:            "CloseLong",
			Price:             100,
			Size:              0.001,
			RealizedPnlAmount: -0.1,
			TransactionUnixMs: time.Date(2026, 4, 11, 21, 11, 0, 0, time.Local).UnixMilli(),
		})
	}
	name := func(string) string { return "BTC/USD" }
	p0 := formatRecentTrades(items, 0, name)
	if !strings.Contains(p0, "第 1/3 页") || !strings.Contains(p0, "*平多*") {
		t.Fatalf("page0 header/layout: %q", p0)
	}
	if !strings.Contains(p0, telegramLineNoListPrefix+"1. *平多*") {
		t.Fatalf("want global rank 1 on page0: %q", p0)
	}
	if strings.Contains(p0, "\\.") {
		t.Fatalf("should not use list-escape backslash-dot: %q", p0)
	}
	if c := strings.Count(p0, "*平多*"); c != TradesPageSize {
		t.Fatalf("want %d trades on page0, count *平多*=%d", TradesPageSize, c)
	}
	p1 := formatRecentTrades(items, 1, name)
	if !strings.Contains(p1, "第 2/3 页") {
		t.Fatalf("page1: %q", p1)
	}
	if !strings.Contains(p1, telegramLineNoListPrefix+"6. *平多*") {
		t.Fatalf("want global rank 6 on page1: %q", p1)
	}
	if c := strings.Count(p1, "*平多*"); c != TradesPageSize {
		t.Fatalf("want %d trades on page1, got %d", TradesPageSize, c)
	}
	p2 := formatRecentTrades(items, 2, name)
	if !strings.Contains(p2, "第 3/3 页") {
		t.Fatalf("page2: %q", p2)
	}
	if c := strings.Count(p2, "*平多*"); c != 2 {
		t.Fatalf("want 2 trades on page2, got %d", c)
	}
}

func TestFormatTradeFromHistory_noSource_cardLayout(t *testing.T) {
	tr := api.TradeHistoryItem{
		Action:                "CloseLong",
		Source:                "OrderFill",
		Price:                 100_000.5,
		Size:                  0.001,
		RealizedPnlAmount:     12.34,
		RealizedFundingAmount: -0.01,
		FeeAmount:             0.02,
		TransactionUnixMs:     time.Date(2026, 4, 11, 12, 34, 56, 0, time.Local).UnixMilli(),
	}
	got := formatTradeFromHistory(tr, "BTC/USD", "0xdeadbeef")
	for _, bad := range []string{"来源", "OrderFill"} {
		if strings.Contains(got, bad) {
			t.Fatalf("must not contain %q:\n%s", bad, got)
		}
	}
	for _, want := range []string{
		"*✅ 平仓成交*",
		"*BTC/USD*",
		"*平多*",
		"成交价 `$100000.5000`",
		"数量 `0.0010`",
		"实现盈亏 `$12.3400`",
		"资金费 `$-0.0100`",
		"手续费 `$0.0200`",
		"2026-04-11 12:34:56",
		"tx:",
		"0xdeadbeef",
		"_本条为平仓结果，不再更新。点「刷新」在下方新消息查看仓位。_",
	} {
		if !strings.Contains(got, want) {
			t.Fatalf("missing %q in:\n%s", want, got)
		}
	}
}
