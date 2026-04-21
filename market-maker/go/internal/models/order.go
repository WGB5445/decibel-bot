package models

import (
	"time"

	"github.com/shopspring/decimal"
)

// Side represents order side
type Side string

const (
	SideBuy  Side = "BUY"
	SideSell Side = "SELL"
)

// OrderStatus represents the lifecycle status of an order
type OrderStatus string

const (
	OrderStatusPendingNew   OrderStatus = "PENDING_NEW"
	OrderStatusNew          OrderStatus = "NEW"
	OrderStatusPartiallyFilled OrderStatus = "PARTIALLY_FILLED"
	OrderStatusFilled       OrderStatus = "FILLED"
	OrderStatusCancelled    OrderStatus = "CANCELLED"
	OrderStatusRejected     OrderStatus = "REJECTED"
	OrderStatusExpired      OrderStatus = "EXPIRED"
)

// LocalOrder represents an in-memory tracked order
type LocalOrder struct {
	ClientOrderID string
	OrderID       string
	MarketAddr    string
	Side          Side
	Price         decimal.Decimal
	Size          decimal.Decimal
	FilledSize    decimal.Decimal
	AvgPrice      decimal.Decimal
	Status        OrderStatus
	IsReduceOnly  bool
	CreatedAt     time.Time
	UpdatedAt     time.Time
	TxHash        string
}

// IsDone returns true if the order is in a terminal state
func (o *LocalOrder) IsDone() bool {
	switch o.Status {
	case OrderStatusFilled, OrderStatusCancelled, OrderStatusRejected, OrderStatusExpired:
		return true
	default:
		return false
	}
}

// RemainingSize returns the unfilled portion
func (o *LocalOrder) RemainingSize() decimal.Decimal {
	return o.Size.Sub(o.FilledSize)
}
