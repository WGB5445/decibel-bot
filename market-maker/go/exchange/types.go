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
	// Optional REST /markets metadata (zero values if unknown).
	MaxLeverage             int
	Mode                    string
	MaxOpenInterest         float64
	UnrealizedPnlHaircutBps int
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
	MarketID                  string
	Size                      float64 // positive = long, negative = short
	EntryPrice                float64 // exchange-reported avg entry (human price)
	UserLeverage              float64
	UnrealizedFunding         float64
	EstimatedLiquidationPrice float64
	IsIsolated                bool
	TransactionVersion        int64
	IsDeleted                 bool
}

// OpenOrder represents a single resting order.
type OpenOrder struct {
	OrderID  string
	MarketID string
}

// PlaceOrderOutcome is returned after a successful on-chain place_order
// (VM success). OrderID may be empty if events could not be parsed.
type PlaceOrderOutcome struct {
	TxHash  string
	OrderID string
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

// BulkOrderEntry is a single price level within a PlaceBulkOrders call.
type BulkOrderEntry struct {
	Price float64
	Size  float64
}
