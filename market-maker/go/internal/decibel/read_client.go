package decibel

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"strings"
	"time"

	"golang.org/x/time/rate"
)

// ReadClient interacts with Decibel REST API (read-only)
type ReadClient struct {
	baseURL    string
	bearerToken string
	httpClient *http.Client
	limiter    *rate.Limiter
}

// NewReadClient creates a new read client
func NewReadClient(baseURL, bearerToken string) *ReadClient {
	return &ReadClient{
		baseURL:     baseURL,
		bearerToken: bearerToken,
		httpClient: &http.Client{
			Timeout: 10 * time.Second,
		},
		limiter: rate.NewLimiter(rate.Every(time.Second), 20), // 20 req/s burst
	}
}

func (c *ReadClient) doRequest(ctx context.Context, method, path string, query url.Values) (*http.Response, error) {
	if err := c.limiter.Wait(ctx); err != nil {
		return nil, err
	}

	u, err := url.Parse(c.baseURL + path)
	if err != nil {
		return nil, err
	}
	if query != nil {
		u.RawQuery = query.Encode()
	}

	req, err := http.NewRequestWithContext(ctx, method, u.String(), nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+c.bearerToken)
	req.Header.Set("Origin", "https://app.decibel.trade/trade")
	req.Header.Set("Accept", "application/json")

	return c.httpClient.Do(req)
}

func (c *ReadClient) decodeJSON(resp *http.Response, out interface{}) error {
	defer resp.Body.Close()
	if resp.StatusCode >= 300 {
		return fmt.Errorf("http %d", resp.StatusCode)
	}
	return json.NewDecoder(resp.Body).Decode(out)
}

// GetMarkets returns all available markets
func (c *ReadClient) GetMarkets(ctx context.Context) ([]Market, error) {
	resp, err := c.doRequest(ctx, http.MethodGet, "/api/v1/markets", nil)
	if err != nil {
		return nil, err
	}
	var out []Market
	if err := c.decodeJSON(resp, &out); err != nil {
		return nil, err
	}
	return out, nil
}

// GetPrices returns current market prices
func (c *ReadClient) GetPrices(ctx context.Context) ([]MarketPrice, error) {
	resp, err := c.doRequest(ctx, http.MethodGet, "/api/v1/prices", nil)
	if err != nil {
		return nil, err
	}
	var out []MarketPrice
	if err := c.decodeJSON(resp, &out); err != nil {
		return nil, err
	}
	return out, nil
}

// GetDepth returns order book depth for a market
func (c *ReadClient) GetDepth(ctx context.Context, marketAddr string, level int) (*MarketDepth, error) {
	q := url.Values{}
	q.Set("market", marketAddr)
	if level > 0 {
		q.Set("depth", fmt.Sprintf("%d", level))
	}
	resp, err := c.doRequest(ctx, http.MethodGet, "/api/v1/depth", q)
	if err != nil {
		return nil, err
	}
	var out MarketDepth
	if err := c.decodeJSON(resp, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetAccountOverview returns account summary
func (c *ReadClient) GetAccountOverview(ctx context.Context, subaccountAddr string) (*AccountOverview, error) {
	q := url.Values{}
	q.Set("subaccount", subaccountAddr)
	resp, err := c.doRequest(ctx, http.MethodGet, "/api/v1/account_overviews", q)
	if err != nil {
		return nil, err
	}
	var out []AccountOverview
	if err := c.decodeJSON(resp, &out); err != nil {
		return nil, err
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("no account overview found")
	}
	return &out[0], nil
}

// GetPositions returns open positions for a subaccount
func (c *ReadClient) GetPositions(ctx context.Context, subaccountAddr string) ([]Position, error) {
	q := url.Values{}
	q.Set("subaccount", subaccountAddr)
	resp, err := c.doRequest(ctx, http.MethodGet, "/api/v1/account_positions", q)
	if err != nil {
		return nil, err
	}
	var out []Position
	if err := c.decodeJSON(resp, &out); err != nil {
		return nil, err
	}
	return out, nil
}

// GetOpenOrders returns currently open orders for a subaccount
func (c *ReadClient) GetOpenOrders(ctx context.Context, subaccountAddr string) ([]OpenOrder, error) {
	q := url.Values{}
	q.Set("subaccount", subaccountAddr)
	resp, err := c.doRequest(ctx, http.MethodGet, "/api/v1/open_orders", q)
	if err != nil {
		return nil, err
	}
	var out []OpenOrder
	if err := c.decodeJSON(resp, &out); err != nil {
		return nil, err
	}
	return out, nil
}

// GetSubaccounts returns subaccounts for a wallet
func (c *ReadClient) GetSubaccounts(ctx context.Context, walletAddr string) ([]Subaccount, error) {
	q := url.Values{}
	q.Set("account", walletAddr)
	resp, err := c.doRequest(ctx, http.MethodGet, "/api/v1/subaccounts", q)
	if err != nil {
		return nil, err
	}
	var out []Subaccount
	if err := c.decodeJSON(resp, &out); err != nil {
		return nil, err
	}
	return out, nil
}

// GetBulkOrders calls GET /bulk_orders for a subaccount and market.
func (c *ReadClient) GetBulkOrders(ctx context.Context, subaccountAddr, marketAddr string) ([]BulkOrderDto, error) {
	if strings.TrimSpace(subaccountAddr) == "" {
		return nil, fmt.Errorf("fetch_bulk_orders: account is required")
	}
	if strings.TrimSpace(marketAddr) == "" {
		return nil, fmt.Errorf("fetch_bulk_orders: market is required")
	}
	q := url.Values{}
	q.Set("account", strings.TrimSpace(subaccountAddr))
	q.Set("market", strings.TrimSpace(marketAddr))
	resp, err := c.doRequest(ctx, http.MethodGet, "/api/v1/bulk_orders", q)
	if err != nil {
		return nil, err
	}
	var rows []BulkOrderDto
	if err := c.decodeJSON(resp, &rows); err != nil {
		return nil, err
	}
	return rows, nil
}
