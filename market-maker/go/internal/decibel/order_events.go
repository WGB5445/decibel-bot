package decibel

import (
	"encoding/json"
	"fmt"
	"strings"

	aptapi "github.com/aptos-labs/aptos-go-sdk/api"
)

// OrderIDFromEvents scans Aptos transaction events for a Decibel order id
// (u128 decimal string). Returns the first non-empty match.
func OrderIDFromEvents(events []*aptapi.Event) string {
	for _, ev := range events {
		if ev == nil || ev.Data == nil {
			continue
		}
		if s := orderIDFromMap(ev.Data); s != "" {
			return s
		}
	}
	return ""
}

func orderIDFromMap(m map[string]any) string {
	for _, key := range []string{"order_id", "orderId"} {
		if s := anyToOrderIDString(m[key]); s != "" {
			return s
		}
	}
	if nested, ok := m["order"].(map[string]any); ok {
		if s := orderIDFromMap(nested); s != "" {
			return s
		}
	}
	return ""
}

func anyToOrderIDString(v any) string {
	switch t := v.(type) {
	case string:
		s := strings.TrimSpace(t)
		if s != "" && s != "0" {
			return s
		}
	case float64:
		if t == float64(int64(t)) && t >= 0 {
			return fmt.Sprintf("%.0f", t)
		}
	case int64:
		if t >= 0 {
			return fmt.Sprintf("%d", t)
		}
	case uint64:
		return fmt.Sprintf("%d", t)
	case json.Number:
		s := strings.TrimSpace(t.String())
		if s != "" && s != "0" {
			return s
		}
	default:
		return ""
	}
	return ""
}
