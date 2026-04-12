package decibel

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"decibel-mm-bot/api"
	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
)

// newTestExchange builds a minimal DecibelExchange for testing syncBulkSeqFromREST.
// Only cfg.SubaccountAddress, apiClient, and market are needed for the sync path.
func newTestExchange(serverURL string) *DecibelExchange {
	return &DecibelExchange{
		cfg: &config.Config{
			SubaccountAddress: "0xacc",
		},
		apiClient: api.NewClient(serverURL, "tok"),
		market:    &exchange.MarketConfig{MarketID: "0xmarket"},
	}
}

func TestSyncBulkSeqFromREST_empty(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `[]`)
	}))
	defer srv.Close()

	d := newTestExchange(srv.URL)
	if err := d.syncBulkSeqFromREST(context.Background()); err != nil {
		t.Fatal(err)
	}
	if d.bulkSeq != 0 {
		t.Errorf("expected bulkSeq=0 for empty result, got %d", d.bulkSeq)
	}
	if !d.bulkSeqSynced {
		t.Error("expected bulkSeqSynced=true")
	}
}

func TestSyncBulkSeqFromREST_picksMax(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `[{"sequence_number":42},{"sequence_number":17},{"sequence_number":99}]`)
	}))
	defer srv.Close()

	d := newTestExchange(srv.URL)
	if err := d.syncBulkSeqFromREST(context.Background()); err != nil {
		t.Fatal(err)
	}
	if d.bulkSeq != 99 {
		t.Errorf("expected bulkSeq=99, got %d", d.bulkSeq)
	}
}

func TestSyncBulkSeqFromREST_retriesOnError(t *testing.T) {
	attempts := 0
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		attempts++
		if attempts < 3 {
			http.Error(w, "server error", http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `[{"sequence_number":7}]`)
	}))
	defer srv.Close()

	d := newTestExchange(srv.URL)

	// Use a short timeout context so the backoff doesn't stall the test long.
	// The retry backoffs are 1s and 4s; we tolerate up to 10s total.
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	if err := d.syncBulkSeqFromREST(ctx); err != nil {
		t.Fatal(err)
	}
	if d.bulkSeq != 7 {
		t.Errorf("expected bulkSeq=7, got %d", d.bulkSeq)
	}
	if attempts != 3 {
		t.Errorf("expected 3 attempts (2 failures + 1 success), got %d", attempts)
	}
}

func TestSyncBulkSeqFromREST_failsAfterMaxAttempts(t *testing.T) {
	attempts := 0
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		attempts++
		http.Error(w, "always broken", http.StatusServiceUnavailable)
	}))
	defer srv.Close()

	d := newTestExchange(srv.URL)

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	err := d.syncBulkSeqFromREST(ctx)
	if err == nil {
		t.Fatal("expected error after max retries, got nil")
	}
	if attempts != 3 {
		t.Errorf("expected exactly 3 attempts, got %d", attempts)
	}
	if d.bulkSeqSynced {
		t.Error("bulkSeqSynced should remain false after failed sync")
	}
}

func TestSyncBulkSeqFromREST_contextCancelledMidRetry(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, "server error", http.StatusInternalServerError)
	}))
	defer srv.Close()

	d := newTestExchange(srv.URL)

	// Cancel context quickly — should abort during first backoff sleep.
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()

	err := d.syncBulkSeqFromREST(ctx)
	if err == nil {
		t.Fatal("expected error when context cancelled, got nil")
	}
}
