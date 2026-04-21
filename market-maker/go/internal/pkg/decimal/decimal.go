package decimal

import (
	"github.com/shopspring/decimal"
)

// ToU64 converts a decimal to uint64 with given decimal places
func ToU64(d decimal.Decimal, places int32) uint64 {
	scaled := d.Shift(places)
	return uint64(scaled.IntPart())
}

// FromU64 converts a uint64 to decimal with given decimal places
func FromU64(v uint64, places int32) decimal.Decimal {
	d := decimal.NewFromInt(int64(v))
	return d.Shift(-places)
}

// FromFloat converts a float64 to decimal safely
func FromFloat(f float64) decimal.Decimal {
	return decimal.NewFromFloat(f)
}

// FromString parses a decimal string
func FromString(s string) (decimal.Decimal, error) {
	return decimal.NewFromString(s)
}
