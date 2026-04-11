// Package api provides the Decibel REST API client.
package api

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"

	"golang.org/x/sync/errgroup"
)

// ─────────────────────────────────────────────────────────────────────────────
// Response types
// ─────────────────────────────────────────────────────────────────────────────

// AccountOverview is returned by GET /account_overviews.
type AccountOverview struct {
	CrossMarginRatio  float64 `json:"cross_margin_ratio"`
	PerpEquityBalance float64 `json:"perp_equity_balance"`
}

// AccountPosition is one entry from GET /account_positions (extended schema).
type AccountPosition struct {
	Market                    string   `json:"market"`
	User                      string   `json:"user"`
	Size                      float64  `json:"size"` // positive=long, negative=short
	UserLeverage              float64  `json:"user_leverage"`
	EntryPrice                float64  `json:"entry_price"`
	IsIsolated                bool     `json:"is_isolated"`
	IsDeleted                 bool     `json:"is_deleted"`
	UnrealizedFunding         float64  `json:"unrealized_funding"`
	EstimatedLiquidationPrice float64  `json:"estimated_liquidation_price"`
	TransactionVersion        int64    `json:"transaction_version"`
	TPOrderID                 *string  `json:"tp_order_id"`
	TPTriggerPrice            *float64 `json:"tp_trigger_price"`
	TPLimitPrice              *float64 `json:"tp_limit_price"`
	SLOrderID                 *string  `json:"sl_order_id"`
	SLTriggerPrice            *float64 `json:"sl_trigger_price"`
	SLLimitPrice              *float64 `json:"sl_limit_price"`
	HasFixedSizedTpsls        bool     `json:"has_fixed_sized_tpsls"`
}

// OpenOrder is a single entry from GET /open_orders.
type OpenOrder struct {
	OrderID    string `json:"order_id"` // u128 as string
	MarketAddr string `json:"market"`
}

// openOrdersPage is the pagination wrapper for GET /open_orders.
type openOrdersPage struct {
	Items []OpenOrder `json:"items"`
}

// PriceInfo is a single entry from GET /prices (API returns an array).
type PriceInfo struct {
	Market string   `json:"market"`
	MidPx  *float64 `json:"mid_px"`
	MarkPx *float64 `json:"mark_px"`
}

// Mid returns the best available mid price: mid_px first, mark_px fallback.
func (p *PriceInfo) Mid() *float64 {
	if p.MidPx != nil {
		return p.MidPx
	}
	return p.MarkPx
}

// TradeHistoryItem is one element of GET /trade_history "items".
type TradeHistoryItem struct {
	Account               string  `json:"account"`
	Market                string  `json:"market"`
	Action                string  `json:"action"`
	Source                string  `json:"source"`
	TradeID               string  `json:"trade_id"`
	Size                  float64 `json:"size"`
	Price                 float64 `json:"price"`
	IsProfit              bool    `json:"is_profit"`
	RealizedPnlAmount     float64 `json:"realized_pnl_amount"`
	RealizedFundingAmount float64 `json:"realized_funding_amount"`
	IsRebate              bool    `json:"is_rebate"`
	FeeAmount             float64 `json:"fee_amount"`
	OrderID               string  `json:"order_id"`
	ClientOrderID         string  `json:"client_order_id"`
	TransactionUnixMs     int64   `json:"transaction_unix_ms"`
	TransactionVersion    int64   `json:"transaction_version"`
}

// TradeHistoryParams are query parameters for GET /trade_history.
type TradeHistoryParams struct {
	Account        string
	Market         string
	OrderID        string
	Side           string // buy | sell
	StartTimestamp int64  // ms
	EndTimestamp   int64  // ms
	SortKey        string
	SortDir        string
	Limit          int
	Offset         int
}

type tradeHistoryPage struct {
	Items []TradeHistoryItem `json:"items"`
}

// MarketConfig is a single entry from GET /markets.
type MarketConfig struct {
	MarketAddr              string  `json:"market_addr"`
	MarketName              string  `json:"market_name"`
	TickSize                float64 `json:"tick_size"`
	LotSize                 float64 `json:"lot_size"`
	MinSize                 float64 `json:"min_size"`
	PxDecimals              int     `json:"px_decimals"`
	SzDecimals              int     `json:"sz_decimals"`
	MaxLeverage             int     `json:"max_leverage"`
	Mode                    string  `json:"mode"`
	MaxOpenInterest         float64 `json:"max_open_interest"`
	UnrealizedPnlHaircutBps int     `json:"unrealized_pnl_haircut_bps"`
}

// StateSnapshot captures everything the bot needs for one cycle decision.
type StateSnapshot struct {
	MarginUsage  float64
	Equity       float64
	Inventory    float64 // net position for the target market
	OpenOrders   []OpenOrder
	Mid          *float64          // nil = price unavailable
	AllPositions []AccountPosition // all positions across all markets (unfiltered)
}

// ─────────────────────────────────────────────────────────────────────────────
// Client
// ─────────────────────────────────────────────────────────────────────────────

// Client is a REST API client for the Decibel exchange.
type Client struct {
	http        *http.Client
	baseURL     string
	bearerToken string
}

// NewClient creates a Client with a 15-second timeout.
func NewClient(baseURL, bearerToken string) *Client {
	return &Client{
		http:        &http.Client{Timeout: 15 * time.Second},
		baseURL:     strings.TrimRight(baseURL, "/"),
		bearerToken: bearerToken,
	}
}

// ── Internal helpers ──────────────────────────────────────────────────────────

func (c *Client) getJSON(ctx context.Context, path string, dst any) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.baseURL+path, nil)
	if err != nil {
		return fmt.Errorf("build request: %w", err)
	}
	req.Header.Set("Authorization", "Bearer "+c.bearerToken)

	resp, err := c.http.Do(req)
	if err != nil {
		return fmt.Errorf("do request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("HTTP %d: GET %s", resp.StatusCode, path)
	}
	return json.NewDecoder(resp.Body).Decode(dst)
}

// ── Public endpoints ──────────────────────────────────────────────────────────

func (c *Client) FetchOverview(ctx context.Context, subaccount string) (*AccountOverview, error) {
	var v AccountOverview
	if err := c.getJSON(ctx, "/account_overviews?account="+subaccount, &v); err != nil {
		return nil, fmt.Errorf("fetch_overview: %w", err)
	}
	return &v, nil
}

func (c *Client) FetchPositions(ctx context.Context, subaccount string) ([]AccountPosition, error) {
	var v []AccountPosition
	if err := c.getJSON(ctx, "/account_positions?account="+subaccount, &v); err != nil {
		return nil, fmt.Errorf("fetch_positions: %w", err)
	}
	return v, nil
}

func (c *Client) FetchOpenOrders(ctx context.Context, subaccount string) ([]OpenOrder, error) {
	var page openOrdersPage
	if err := c.getJSON(ctx, "/open_orders?account="+subaccount, &page); err != nil {
		return nil, fmt.Errorf("fetch_open_orders: %w", err)
	}
	return page.Items, nil
}

// FetchTradeHistory calls GET /trade_history with the given filters.
func (c *Client) FetchTradeHistory(ctx context.Context, p TradeHistoryParams) ([]TradeHistoryItem, error) {
	if strings.TrimSpace(p.Account) == "" {
		return nil, fmt.Errorf("fetch_trade_history: account is required")
	}
	q := url.Values{}
	q.Set("account", strings.TrimSpace(p.Account))
	if strings.TrimSpace(p.Market) != "" {
		q.Set("market", strings.TrimSpace(p.Market))
	}
	if strings.TrimSpace(p.OrderID) != "" {
		q.Set("order_id", strings.TrimSpace(p.OrderID))
	}
	if strings.TrimSpace(p.Side) != "" {
		q.Set("side", strings.TrimSpace(p.Side))
	}
	if p.StartTimestamp > 0 {
		q.Set("start_timestamp", strconv.FormatInt(p.StartTimestamp, 10))
	}
	if p.EndTimestamp > 0 {
		q.Set("end_timestamp", strconv.FormatInt(p.EndTimestamp, 10))
	}
	if strings.TrimSpace(p.SortKey) != "" {
		q.Set("sort_key", strings.TrimSpace(p.SortKey))
	}
	if strings.TrimSpace(p.SortDir) != "" {
		q.Set("sort_dir", strings.TrimSpace(p.SortDir))
	}
	if p.Limit > 0 {
		q.Set("limit", strconv.Itoa(p.Limit))
	}
	if p.Offset > 0 {
		q.Set("offset", strconv.Itoa(p.Offset))
	}
	path := "/trade_history?" + q.Encode()
	var page tradeHistoryPage
	if err := c.getJSON(ctx, path, &page); err != nil {
		return nil, fmt.Errorf("fetch_trade_history: %w", err)
	}
	return page.Items, nil
}

func (c *Client) FetchPrice(ctx context.Context, marketAddr string) (*PriceInfo, error) {
	var list []PriceInfo
	if err := c.getJSON(ctx, "/prices?market="+marketAddr, &list); err != nil {
		return nil, fmt.Errorf("fetch_price: %w", err)
	}
	for i := range list {
		if AddrEqual(list[i].Market, marketAddr) {
			return &list[i], nil
		}
	}
	return nil, fmt.Errorf("fetch_price: no price entry for %s", marketAddr)
}

func (c *Client) FetchMarkets(ctx context.Context) ([]MarketConfig, error) {
	var v []MarketConfig
	if err := c.getJSON(ctx, "/markets", &v); err != nil {
		return nil, fmt.Errorf("fetch_markets: %w", err)
	}
	// API returns raw chain units; convert to human-readable floats.
	for i := range v {
		pxScale := math.Pow10(v[i].PxDecimals)
		szScale := math.Pow10(v[i].SzDecimals)
		v[i].TickSize /= pxScale
		v[i].LotSize /= szScale
		v[i].MinSize /= szScale
	}
	return v, nil
}

// FindMarket finds a market by name (normalizing "/" ↔ "-", case-insensitive).
func (c *Client) FindMarket(ctx context.Context, name string) (*MarketConfig, error) {
	markets, err := c.FetchMarkets(ctx)
	if err != nil {
		return nil, err
	}
	target := normalizeMarket(name)
	for i, m := range markets {
		if normalizeMarket(m.MarketName) == target {
			return &markets[i], nil
		}
	}
	return nil, fmt.Errorf("market %q not found in /markets response", name)
}

// FetchState fetches overview, positions, open orders, and price IN PARALLEL.
// It filters positions and orders to those matching marketAddr.
func (c *Client) FetchState(ctx context.Context, subaccount, marketAddr string) (*StateSnapshot, error) {
	var (
		overview  *AccountOverview
		positions []AccountPosition
		orders    []OpenOrder
		price     *PriceInfo
	)

	g, gctx := errgroup.WithContext(ctx)

	g.Go(func() error {
		var err error
		overview, err = c.FetchOverview(gctx, subaccount)
		return err
	})
	g.Go(func() error {
		var err error
		positions, err = c.FetchPositions(gctx, subaccount)
		return err
	})
	g.Go(func() error {
		var err error
		orders, err = c.FetchOpenOrders(gctx, subaccount)
		return err
	})
	g.Go(func() error {
		var err error
		price, err = c.FetchPrice(gctx, marketAddr)
		return err
	})

	if err := g.Wait(); err != nil {
		return nil, fmt.Errorf("fetch_state: %w", err)
	}

	// Filter to the target market.
	inventory := 0.0
	for _, p := range positions {
		if AddrEqual(p.Market, marketAddr) {
			inventory = p.Size
			break
		}
	}

	var myOrders []OpenOrder
	for _, o := range orders {
		if AddrEqual(o.MarketAddr, marketAddr) {
			myOrders = append(myOrders, o)
		}
	}

	return &StateSnapshot{
		MarginUsage:  overview.CrossMarginRatio,
		Equity:       overview.PerpEquityBalance,
		Inventory:    inventory,
		OpenOrders:   myOrders,
		Mid:          price.Mid(),
		AllPositions: positions,
	}, nil
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

// AddrEqual compares two Aptos addresses case-insensitively,
// ignoring leading zeros and the "0x" prefix.
func AddrEqual(a, b string) bool {
	return NormalizeAddr(a) == NormalizeAddr(b)
}

// NormalizeAddr strips the "0x" prefix, lowercases, and removes leading zeros.
func NormalizeAddr(addr string) string {
	s := strings.TrimPrefix(strings.ToLower(addr), "0x")
	s = strings.TrimLeft(s, "0")
	if s == "" {
		return "0"
	}
	return s
}

func normalizeMarket(name string) string {
	return strings.ToUpper(strings.ReplaceAll(name, "/", "-"))
}
