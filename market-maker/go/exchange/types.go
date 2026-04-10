package exchange

// MarketConfig holds exchange-agnostic market metadata.
type MarketConfig struct {
	MarketID   string  // exchange-specific unique ID (Decibel uses an address)
	MarketName string  // human-readable name, e.g. "BTC/USD"
	TickSize   float64 // minimum price increment (human-readable)
	LotSize    float64 // minimum size increment (human-readable)
	MinSize    float64 // minimum order size (human-readable)
	PxDecimals int     // price scaling decimals (for integer encoding)
	SzDecimals int     // size scaling decimals (for integer encoding)
}

// StateSnapshot captures everything the strategy needs for one cycle decision.
type StateSnapshot struct {
	Equity       float64
	MarginUsage  float64
	Inventory    float64     // net position for the target market
	Mid          *float64    // nil = price unavailable
	OpenOrders   []OpenOrder // open orders for the target market
	AllPositions []Position  // all positions across all markets
}

// Position represents a single market position.
type Position struct {
	MarketID string
	Size     float64 // positive = long, negative = short
}

// OpenOrder represents a single resting order.
type OpenOrder struct {
	OrderID  string
	MarketID string
}

// PlaceOrderRequest describes an order to be placed.
type PlaceOrderRequest struct {
	MarketID    string
	Price       float64
	Size        float64
	IsBuy       bool
	TimeInForce int // 0=GTC, 1=POST_ONLY, 2=IOC
	ReduceOnly  bool
}
