package decimal

import (
	"testing"

	"github.com/shopspring/decimal"
)

func TestToU64(t *testing.T) {
	d := decimal.NewFromFloat(5.67)
	got := ToU64(d, 9)
	want := uint64(5_670_000_000)
	if got != want {
		t.Fatalf("ToU64(5.67, 9) = %d, want %d", got, want)
	}
}

func TestFromU64(t *testing.T) {
	got := FromU64(1_000_000_000, 9)
	want := decimal.NewFromInt(1)
	if !got.Equal(want) {
		t.Fatalf("FromU64(1e9, 9) = %s, want %s", got.String(), want.String())
	}
}
