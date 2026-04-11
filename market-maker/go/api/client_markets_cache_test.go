package api

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
)

func TestFetchMarketsSecondCallUsesCache(t *testing.T) {
	var n int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/markets" {
			http.NotFound(w, r)
			return
		}
		atomic.AddInt32(&n, 1)
		w.Header().Set("Content-Type", "application/json")
		_, _ = fmt.Fprint(w, `[{"market_addr":"0x1","market_name":"BTC/USD","tick_size":100,"lot_size":1000,"min_size":2000,"px_decimals":2,"sz_decimals":5,"max_leverage":10,"mode":"","max_open_interest":0,"unrealized_pnl_haircut_bps":0}]`)
	}))
	defer srv.Close()

	c := NewClient(srv.URL, "tok")
	if _, err := c.FetchMarkets(context.Background()); err != nil {
		t.Fatal(err)
	}
	if _, err := c.FetchMarkets(context.Background()); err != nil {
		t.Fatal(err)
	}
	if atomic.LoadInt32(&n) != 1 {
		t.Fatalf("want 1 GET /markets, got %d", n)
	}
}

func TestFindMarketThenFetchMarketsUsesCache(t *testing.T) {
	var n int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/markets" {
			http.NotFound(w, r)
			return
		}
		atomic.AddInt32(&n, 1)
		w.Header().Set("Content-Type", "application/json")
		_, _ = fmt.Fprint(w, `[{"market_addr":"0x1","market_name":"BTC/USD","tick_size":100,"lot_size":1000,"min_size":2000,"px_decimals":2,"sz_decimals":5,"max_leverage":10,"mode":"","max_open_interest":0,"unrealized_pnl_haircut_bps":0}]`)
	}))
	defer srv.Close()

	c := NewClient(srv.URL, "tok")
	ctx := context.Background()
	if _, err := c.FindMarket(ctx, "BTC/USD"); err != nil {
		t.Fatal(err)
	}
	if _, err := c.FetchMarkets(ctx); err != nil {
		t.Fatal(err)
	}
	if atomic.LoadInt32(&n) != 1 {
		t.Fatalf("want 1 GET /markets, got %d", n)
	}
}
