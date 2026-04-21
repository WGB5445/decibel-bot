package models

import "time"

// EventType categorizes engine events
type EventType int

const (
	EventTick EventType = iota
	EventDepthUpdate
	EventPriceUpdate
	EventOrderUpdate
	EventPositionUpdate
	EventTradeFill
	EventTxConfirmed
	EventTxFailed
)

// Event is the unified event envelope
type Event struct {
	Type EventType
	Data interface{}
	TS   time.Time
}

// DepthUpdate represents an order book update
type DepthUpdate struct {
	MarketAddr string
	Bids       []PriceLevel
	Asks       []PriceLevel
}

// PriceLevel is a single price level in the order book
type PriceLevel struct {
	Price  string
	Amount string
}

// PriceUpdate represents a market price tick
type PriceUpdate struct {
	MarketAddr       string
	OraclePx         string
	MarkPx           string
	MidPx            string
	FundingRateBps   float64
	IsFundingPositive bool
}

// OrderUpdate represents a change in order status
type OrderUpdate struct {
	SubaccountAddr string
	OrderID        string
	Status         string // NEW, PARTIALLY_FILLED, FILLED, CANCELLED, REJECTED
	FilledSize     string
	AvgPrice       string
	RemainingSize  string
}

// PositionUpdate represents a position change
type PositionUpdate struct {
	SubaccountAddr string
	MarketAddr     string
	PositionAmt    string // positive=long, negative=short
	EntryPrice     string
	UnrealizedPnl  string
	Leverage       int
	MarginType     string
}

// TradeFill represents a trade execution
type TradeFill struct {
	SubaccountAddr string
	MarketAddr     string
	OrderID        string
	Price          string
	Size           string
	IsBuy          bool
}

// TxResult represents a transaction confirmation or failure
type TxResult struct {
	Hash    string
	Success bool
	Error   string
	Kind    string
}
