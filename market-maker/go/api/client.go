// Package api provides the Decibel REST API client.
package api

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"math"
	"net/http"
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

// Position is a single entry from GET /account_positions.
type Position struct {
	MarketAddr string  `json:"market"`
	Size       float64 `json:"size"` // positive=long, negative=short
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

// MarketConfig is a single entry from GET /markets.
type MarketConfig struct {
	MarketAddr string  `json:"market_addr"`
	MarketName string  `json:"market_name"`
	TickSize   float64 `json:"tick_size"`
	LotSize    float64 `json:"lot_size"`
	MinSize    float64 `json:"min_size"`
	PxDecimals int     `json:"px_decimals"`
	SzDecimals int     `json:"sz_decimals"`
}

// StateSnapshot captures everything the bot needs for one cycle decision.
type StateSnapshot struct {
	MarginUsage float64
	Equity      float64
	Inventory   float64 // net position for the target market
	OpenOrders  []OpenOrder
	Mid         *float64 // nil = price unavailable
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

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return fmt.Errorf("read response body: %w", err)
	}

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("HTTP %d: GET %s: %s", resp.StatusCode, path, strings.TrimSpace(string(body)))
	}

	if err := json.Unmarshal(body, dst); err != nil {
		return fmt.Errorf("decode JSON: %w; body=%s", err, string(body))
	}
	return nil
}

// ── Public endpoints ──────────────────────────────────────────────────────────

func (c *Client) FetchOverview(ctx context.Context, subaccount string) (*AccountOverview, error) {
	var v AccountOverview
	if err := c.getJSON(ctx, "/account_overviews?account="+subaccount, &v); err != nil {
		return nil, fmt.Errorf("fetch_overview: %w", err)
	}
	return &v, nil
}

func (c *Client) FetchPositions(ctx context.Context, subaccount string) ([]Position, error) {
	var v []Position
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
		positions []Position
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
		if AddrEqual(p.MarketAddr, marketAddr) {
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
		MarginUsage: overview.CrossMarginRatio,
		Equity:      overview.PerpEquityBalance,
		Inventory:   inventory,
		OpenOrders:  myOrders,
		Mid:         price.Mid(),
	}, nil
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

// AddrEqual compares two Aptos addresses case-insensitively,
// ignoring leading zeros and the "0x" prefix.
func AddrEqual(a, b string) bool {
	return normalizeAddr(a) == normalizeAddr(b)
}

func normalizeAddr(addr string) string {
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
