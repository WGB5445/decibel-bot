package aptos

import (
	"testing"

	aptapi "github.com/aptos-labs/aptos-go-sdk/api"
)

func TestOrderIDFromEvents(t *testing.T) {
	ev := &aptapi.Event{
		Type: "0xpkg::m::OrderPlaced",
		Data: map[string]any{"order_id": "170141599249866106957464978622550900736"},
	}
	if got := OrderIDFromEvents([]*aptapi.Event{ev}); got != "170141599249866106957464978622550900736" {
		t.Fatalf("got %q", got)
	}
	if got := OrderIDFromEvents(nil); got != "" {
		t.Fatalf("expected empty, got %q", got)
	}
}
