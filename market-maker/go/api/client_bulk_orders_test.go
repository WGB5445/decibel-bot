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
	if len(rows) != 1 || rows[0].SequenceNumber != 57 || rows[0].PreviousSeqNum != 56 {
		t.Fatalf("got %#v", rows)
	}
}
