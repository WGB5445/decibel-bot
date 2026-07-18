package webui

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"decibel-mm-bot/api"
	"decibel-mm-bot/botstate"
	"decibel-mm-bot/exchange"
)

// fakeInfo implements notify.InfoProvider for testing.
type fakeInfo struct {
	snap                botstate.Snapshot
	fetchErr            error
	maxInventory        float64
	maxMarginUsage      float64
	flattenForceSeconds float64
	dryRun              bool

	pauseCalls   []bool // recorded cancelResting args
	resumeCalls  int
	flattenCalls int
	flattenErr   error
}

func (f *fakeInfo) GetSnapshot() botstate.Snapshot { return f.snap }
func (f *fakeInfo) FetchLiveSnapshot(_ context.Context) (botstate.Snapshot, error) {
	if f.fetchErr != nil {
		return botstate.Snapshot{}, f.fetchErr
	}
	return f.snap, nil
}
func (f *fakeInfo) FlattenPosition(_ context.Context) (exchange.PlaceOrderOutcome, error) {
	f.flattenCalls++
	if f.flattenErr != nil {
		return exchange.PlaceOrderOutcome{}, f.flattenErr
	}
	return exchange.PlaceOrderOutcome{TxHash: "0xtx", OrderID: "order-1"}, nil
}
func (f *fakeInfo) DryRun() bool { return f.dryRun }
func (f *fakeInfo) FetchTradeHistoryByOrder(_ context.Context, _, _ string) ([]api.TradeHistoryItem, error) {
	return nil, nil
}
func (f *fakeInfo) FetchRecentTrades(_ context.Context, _ int) ([]api.TradeHistoryItem, error) {
	return nil, nil
}
func (f *fakeInfo) GasBalance(_ context.Context) (float64, string, error) { return 1, "APT", nil }
func (f *fakeInfo) WalletAddress() string                                 { return "0xwallet" }
func (f *fakeInfo) MaxInventory() float64                                 { return f.maxInventory }
func (f *fakeInfo) MarketDisplayName(addr string) string                  { return addr }
func (f *fakeInfo) FlattenForceSeconds() float64                          { return f.flattenForceSeconds }
func (f *fakeInfo) MaxMarginUsage() float64                               { return f.maxMarginUsage }
func (f *fakeInfo) PauseTrading(_ context.Context, cancelResting bool) error {
	f.pauseCalls = append(f.pauseCalls, cancelResting)
	return nil
}
func (f *fakeInfo) ResumeTrading() { f.resumeCalls++ }

func newTestServer(info *fakeInfo, token string) *Server {
	return New(Config{Bind: "127.0.0.1:0", Token: token}, info)
}

func TestStatusRequiresToken(t *testing.T) {
	info := &fakeInfo{snap: botstate.Snapshot{TargetMarketName: "BTC/USD"}}
	srv := newTestServer(info, "secret")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	res, err := http.Get(ts.URL + "/api/status")
	if err != nil {
		t.Fatalf("GET: %v", err)
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusUnauthorized {
		t.Errorf("expected 401 without token, got %d", res.StatusCode)
	}
}

func TestStatusWithValidToken(t *testing.T) {
	mid := 100_000.0
	info := &fakeInfo{
		snap: botstate.Snapshot{
			TargetMarketName:   "BTC/USD",
			Inventory:          0.002,
			Mid:                &mid,
			Paused:             true,
			PauseCancelResting: true,
		},
		maxInventory:        0.005,
		maxMarginUsage:      0.5,
		flattenForceSeconds: 240,
	}
	srv := newTestServer(info, "secret")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	res, err := http.Get(ts.URL + "/api/status?token=secret")
	if err != nil {
		t.Fatalf("GET: %v", err)
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		t.Fatalf("expected 200, got %d", res.StatusCode)
	}
	var payload statusPayload
	if err := json.NewDecoder(res.Body).Decode(&payload); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if payload.MarketName != "BTC/USD" {
		t.Errorf("market_name = %q, want BTC/USD", payload.MarketName)
	}
	if !payload.Paused || !payload.PauseCancelResting {
		t.Errorf("expected paused=true, pause_cancel_resting=true, got %+v", payload)
	}
	if payload.MaxInventory != 0.005 {
		t.Errorf("max_inventory = %v, want 0.005", payload.MaxInventory)
	}
	if payload.Mid == nil || *payload.Mid != mid {
		t.Errorf("mid = %v, want %v", payload.Mid, mid)
	}
}

func TestBearerTokenAuthAlsoWorks(t *testing.T) {
	info := &fakeInfo{snap: botstate.Snapshot{}}
	srv := newTestServer(info, "secret")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	req, _ := http.NewRequest(http.MethodGet, ts.URL+"/api/status", nil)
	req.Header.Set("Authorization", "Bearer secret")
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatalf("GET: %v", err)
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		t.Errorf("expected 200 with valid bearer token, got %d", res.StatusCode)
	}
}

func TestPauseEndpointCallsPauseTrading(t *testing.T) {
	info := &fakeInfo{snap: botstate.Snapshot{}}
	srv := newTestServer(info, "secret")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	res, err := http.Post(ts.URL+"/api/pause?token=secret", "application/json", strings.NewReader(`{"cancel_resting":true}`))
	if err != nil {
		t.Fatalf("POST: %v", err)
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		t.Fatalf("expected 200, got %d", res.StatusCode)
	}
	if len(info.pauseCalls) != 1 || info.pauseCalls[0] != true {
		t.Errorf("expected PauseTrading(true) called once, got %v", info.pauseCalls)
	}
}

func TestResumeEndpointCallsResumeTrading(t *testing.T) {
	info := &fakeInfo{snap: botstate.Snapshot{}}
	srv := newTestServer(info, "secret")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	res, err := http.Post(ts.URL+"/api/resume?token=secret", "application/json", nil)
	if err != nil {
		t.Fatalf("POST: %v", err)
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		t.Fatalf("expected 200, got %d", res.StatusCode)
	}
	if info.resumeCalls != 1 {
		t.Errorf("expected ResumeTrading called once, got %d", info.resumeCalls)
	}
}

func TestFlattenEndpointReturnsOutcome(t *testing.T) {
	info := &fakeInfo{snap: botstate.Snapshot{}}
	srv := newTestServer(info, "secret")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	res, err := http.Post(ts.URL+"/api/flatten?token=secret", "application/json", nil)
	if err != nil {
		t.Fatalf("POST: %v", err)
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		t.Fatalf("expected 200, got %d", res.StatusCode)
	}
	var out map[string]any
	if err := json.NewDecoder(res.Body).Decode(&out); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if out["tx_hash"] != "0xtx" || out["order_id"] != "order-1" {
		t.Errorf("unexpected flatten response: %+v", out)
	}
	if info.flattenCalls != 1 {
		t.Errorf("expected FlattenPosition called once, got %d", info.flattenCalls)
	}
}

func TestGetRequestsToMutatingEndpointsAreRejected(t *testing.T) {
	info := &fakeInfo{snap: botstate.Snapshot{}}
	srv := newTestServer(info, "secret")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	res, err := http.Get(ts.URL + "/api/pause?token=secret")
	if err != nil {
		t.Fatalf("GET: %v", err)
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusMethodNotAllowed {
		t.Errorf("expected 405 for GET /api/pause, got %d", res.StatusCode)
	}
}

func TestRunRejectsEmptyToken(t *testing.T) {
	info := &fakeInfo{snap: botstate.Snapshot{}}
	srv := New(Config{Bind: "127.0.0.1:0", Token: ""}, info)
	if err := srv.Run(context.Background()); err == nil {
		t.Error("expected Run to reject an empty token")
	}
}
