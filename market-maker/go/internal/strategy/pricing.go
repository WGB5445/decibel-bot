package strategy

import (
	"sync"

	"github.com/shopspring/decimal"
)

// PriceType defines how reference price is derived
type PriceType string

const (
	PriceTypeMidPrice    PriceType = "mid_price"
	PriceTypeBestBid     PriceType = "best_bid"
	PriceTypeBestAsk     PriceType = "best_ask"
	PriceTypeLastTrade   PriceType = "last_trade"
	PriceTypeOracle      PriceType = "oracle"
	PriceTypeMark        PriceType = "mark"
)

// Pricing holds current market price state
type Pricing struct {
	midPrice    decimal.Decimal
	bestBid     decimal.Decimal
	bestAsk     decimal.Decimal
	lastTrade   decimal.Decimal
	oraclePrice decimal.Decimal
	markPrice   decimal.Decimal
	mu          sync.RWMutex
}

// NewPricing creates a pricing helper
func NewPricing() *Pricing {
	return &Pricing{}
}

// UpdateDepth updates bid/ask from order book
func (p *Pricing) UpdateDepth(bestBid, bestAsk decimal.Decimal) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.bestBid = bestBid
	p.bestAsk = bestAsk
	p.midPrice = bestBid.Add(bestAsk).Div(decimal.NewFromInt(2))
}

// UpdateMarketPrice updates from market price tick
func (p *Pricing) UpdateMarketPrice(mid, oracle, mark, lastTrade decimal.Decimal) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.midPrice = mid
	p.oraclePrice = oracle
	p.markPrice = mark
	p.lastTrade = lastTrade
}

// GetPrice returns the reference price by type
func (p *Pricing) GetPrice(pt PriceType) decimal.Decimal {
	p.mu.RLock()
	defer p.mu.RUnlock()
	switch pt {
	case PriceTypeBestBid:
		return p.bestBid
	case PriceTypeBestAsk:
		return p.bestAsk
	case PriceTypeLastTrade:
		return p.lastTrade
	case PriceTypeOracle:
		return p.oraclePrice
	case PriceTypeMark:
		return p.markPrice
	default:
		return p.midPrice
	}
}

// BestBid returns current best bid
func (p *Pricing) BestBid() decimal.Decimal {
	p.mu.RLock()
	defer p.mu.RUnlock()
	return p.bestBid
}

// BestAsk returns current best ask
func (p *Pricing) BestAsk() decimal.Decimal {
	p.mu.RLock()
	defer p.mu.RUnlock()
	return p.bestAsk
}

// MidPrice returns current mid price
func (p *Pricing) MidPrice() decimal.Decimal {
	p.mu.RLock()
	defer p.mu.RUnlock()
	return p.midPrice
}
