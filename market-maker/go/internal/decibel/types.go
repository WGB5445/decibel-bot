package decibel

import (
	"github.com/shopspring/decimal"
)

// Market represents a Decibel trading market
type Market struct {
	MarketAddr  string `json:"market"`
	Name        string `json:"name"`
	BaseAsset   string `json:"base_asset"`
	QuoteAsset  string `json:"quote_asset"`
	PriceDecimals int  `json:"price_decimals"`
	SizeDecimals  int  `json:"size_decimals"`
}

// MarketPrice represents current market price data
type MarketPrice struct {
	Market            string  `json:"market"`
	OraclePx          float64 `json:"oracle_px"`
	MarkPx            float64 `json:"mark_px"`
	MidPx             float64 `json:"mid_px"`
	FundingRateBps    float64 `json:"funding_rate_bps"`
	IsFundingPositive bool    `json:"is_funding_positive"`
	TransactionUnixMs int64   `json:"transaction_unix_ms"`
	OpenInterest      float64 `json:"open_interest"`
}

// MarketDepth represents order book snapshot
type MarketDepth struct {
	Market string       `json:"market"`
	Bids   []DepthLevel `json:"bids"`
	Asks   []DepthLevel `json:"asks"`
}

// DepthLevel is a single level in the order book
type DepthLevel struct {
	Price  string `json:"px"`
	Amount string `json:"sz"`
}

// AccountOverview represents account summary
type AccountOverview struct {
	SubaccountAddr string  `json:"subaccount"`
	Equity         float64 `json:"equity"`
	AvailableMargin float64 `json:"available_margin"`
	UsedMargin     float64 `json:"used_margin"`
	UnrealizedPnl  float64 `json:"unrealized_pnl"`
}

// OpenOrder represents an active order from REST API
type OpenOrder struct {
	OrderID        string `json:"order_id"`
	SubaccountAddr string `json:"subaccount"`
	MarketAddr     string `json:"market"`
	Price          string `json:"price"`
	Size           string `json:"size"`
	FilledSize     string `json:"filled_size"`
	RemainingSize  string `json:"remaining_size"`
	Side           string `json:"side"`
	Status         string `json:"status"`
	IsReduceOnly   bool   `json:"is_reduce_only"`
	CreatedAt      int64  `json:"created_at"`
}

// Position represents a position from REST API
type Position struct {
	SubaccountAddr   string `json:"subaccount"`
	MarketAddr       string `json:"market"`
	PositionSize     string `json:"position_size"`
	EntryPrice       string `json:"entry_price"`
	Leverage         int    `json:"leverage"`
	MarginType       string `json:"margin_type"`
	UnrealizedPnl    string `json:"unrealized_pnl"`
	LiquidationPrice string `json:"liquidation_price"`
}

// Subaccount represents a trading subaccount
type Subaccount struct {
	Address string `json:"subaccount"`
	IsPrimary bool `json:"is_primary"`
}

// PlaceOrderRequest represents a request to place an order on-chain
type PlaceOrderRequest struct {
	PackageAddr    string
	SubaccountAddr string
	MarketAddr     string
	Price          decimal.Decimal
	Size           decimal.Decimal
	IsBuy          bool
	TimeInForce    TimeInForce
	IsReduceOnly   bool
	ClientOrderID  *uint64
}

// BulkOrderRequest represents a single order within a bulk order transaction
type BulkOrderRequest struct {
	MarketAddr     string
	Price          decimal.Decimal
	Size           decimal.Decimal
	IsBuy          bool
	TimeInForce    TimeInForce
	IsReduceOnly   bool
}

// PlaceOrderOutcome captures the result of an on-chain place order.
type PlaceOrderOutcome struct {
	TxHash  string
	OrderID string
}

// BulkOrderDto is one element of GET /bulk_orders.
type BulkOrderDto struct {
	SequenceNumber uint64    `json:"sequence_number"`
	PreviousSeqNum *uint64   `json:"previous_seq_num"`
	BidSizes       []float64 `json:"bid_sizes"`
	AskSizes       []float64 `json:"ask_sizes"`
}

// HasRestingQuotes reports whether this snapshot still has non-zero bid or ask size on the book.
func (b *BulkOrderDto) HasRestingQuotes() bool {
	if b == nil {
		return false
	}
	const eps = 1e-12
	for _, s := range b.BidSizes {
		if s > eps {
			return true
		}
	}
	for _, s := range b.AskSizes {
		if s > eps {
			return true
		}
	}
	return false
}

// WSAuthMessage represents the WebSocket authentication message
// (exact format may vary based on Decibel protocol)
type WSAuthMessage struct {
	Type  string `json:"type"`
	Token string `json:"token"`
}

// WSSubscribeMessage represents a subscription request
type WSSubscribeMessage struct {
	Type    string `json:"type"`
	Channel string `json:"channel"`
}
