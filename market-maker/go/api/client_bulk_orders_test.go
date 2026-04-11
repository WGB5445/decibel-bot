package api

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestFetchBulkOrders_decodesSequence(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/bulk_orders" {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = fmt.Fprint(w, `[{"sequence_number":57,"previous_seq_num":56}]`)
	}))
	defer srv.Close()

	c := NewClient(srv.URL, "tok")
	rows, err := c.FetchBulkOrders(context.Background(), "0xacc", "0xmark")
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 || rows[0].SequenceNumber != 57 ||
		rows[0].PreviousSeqNum == nil || *rows[0].PreviousSeqNum != 56 {
		t.Fatalf("got %#v", rows)
	}
	if rows[0].HasRestingQuotes() {
		t.Fatal("expected no resting quotes when sizes absent")
	}
}

func TestBulkOrderDto_HasRestingQuotes(t *testing.T) {
	var b BulkOrderDto
	if b.HasRestingQuotes() {
		t.Fatal("zero value should not be active")
	}
	bid := 1.0
	b = BulkOrderDto{BidSizes: []float64{bid}}
	if !b.HasRestingQuotes() {
		t.Fatal("expected active bid")
	}
	b = BulkOrderDto{AskSizes: []float64{0.0001}}
	if !b.HasRestingQuotes() {
		t.Fatal("expected active ask")
	}
	b = BulkOrderDto{BidSizes: []float64{0}, AskSizes: []float64{0}}
	if b.HasRestingQuotes() {
		t.Fatal("zeros should not be active")
	}
}
