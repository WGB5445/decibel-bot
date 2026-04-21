package strategy

import (
	"github.com/bujih/decibel-mm-go/internal/decibel"
	"github.com/shopspring/decimal"
)

// PriceSize represents a proposed order
type PriceSize struct {
	Price decimal.Decimal
	Size  decimal.Decimal
}

// Proposal contains desired buy and sell orders
type Proposal struct {
	Buys  []PriceSize
	Sells []PriceSize
}

// ProposalBuilder constructs order proposals
type ProposalBuilder struct {
	bidSpread        decimal.Decimal
	askSpread        decimal.Decimal
	orderAmount      decimal.Decimal
	orderLevels      int
	orderLevelSpread decimal.Decimal
	orderLevelAmount decimal.Decimal
	priceCeiling     decimal.Decimal
	priceFloor       decimal.Decimal
	minSpread        decimal.Decimal
	priceDecimals    int32
	sizeDecimals     int32
}

// NewProposalBuilder creates a proposal builder
func NewProposalBuilder(
	bidSpread, askSpread, orderAmount decimal.Decimal,
	orderLevels int,
	orderLevelSpread, orderLevelAmount, priceCeiling, priceFloor, minSpread decimal.Decimal,
	priceDecimals, sizeDecimals int32,
) *ProposalBuilder {
	return &ProposalBuilder{
		bidSpread:        bidSpread,
		askSpread:        askSpread,
		orderAmount:      orderAmount,
		orderLevels:      orderLevels,
		orderLevelSpread: orderLevelSpread,
		orderLevelAmount: orderLevelAmount,
		priceCeiling:     priceCeiling,
		priceFloor:       priceFloor,
		minSpread:        minSpread,
		priceDecimals:    priceDecimals,
		sizeDecimals:     sizeDecimals,
	}
}

// CreateBaseProposal generates the raw proposal around reference price
func (pb *ProposalBuilder) CreateBaseProposal(refPrice decimal.Decimal) *Proposal {
	buys := make([]PriceSize, 0, pb.orderLevels)
	sells := make([]PriceSize, 0, pb.orderLevels)

	for i := 0; i < pb.orderLevels; i++ {
		level := decimal.NewFromInt(int64(i))

		buyPrice := refPrice.Mul(
			decimal.NewFromInt(1).Sub(pb.bidSpread).Sub(level.Mul(pb.orderLevelSpread)),
		)
		sellPrice := refPrice.Mul(
			decimal.NewFromInt(1).Add(pb.askSpread).Add(level.Mul(pb.orderLevelSpread)),
		)
		size := pb.orderAmount.Add(level.Mul(pb.orderLevelAmount))

		buyPrice = quantize(buyPrice, pb.priceDecimals)
		sellPrice = quantize(sellPrice, pb.priceDecimals)
		size = quantize(size, pb.sizeDecimals)

		if size.GreaterThan(decimal.Zero) {
			buys = append(buys, PriceSize{Price: buyPrice, Size: size})
			sells = append(sells, PriceSize{Price: sellPrice, Size: size})
		}
	}

	return &Proposal{Buys: buys, Sells: sells}
}

// ApplyPriceBand removes buys above ceiling and sells below floor
func (pb *ProposalBuilder) ApplyPriceBand(refPrice decimal.Decimal, p *Proposal) {
	if pb.priceCeiling.GreaterThan(decimal.Zero) && refPrice.GreaterThanOrEqual(pb.priceCeiling) {
		p.Buys = nil
	}
	if pb.priceFloor.GreaterThan(decimal.Zero) && refPrice.LessThanOrEqual(pb.priceFloor) {
		p.Sells = nil
	}
}

// FilterOutTakers removes orders that would cross the spread
func (pb *ProposalBuilder) FilterOutTakers(bestBid, bestAsk decimal.Decimal, p *Proposal) {
	if !bestAsk.IsZero() {
		filtered := make([]PriceSize, 0, len(p.Buys))
		for _, b := range p.Buys {
			if b.Price.LessThan(bestAsk) {
				filtered = append(filtered, b)
			}
		}
		p.Buys = filtered
	}
	if !bestBid.IsZero() {
		filtered := make([]PriceSize, 0, len(p.Sells))
		for _, s := range p.Sells {
			if s.Price.GreaterThan(bestBid) {
				filtered = append(filtered, s)
			}
		}
		p.Sells = filtered
	}
}

// ApplyOrderOptimization improves price for single-level orders
func (pb *ProposalBuilder) ApplyOrderOptimization(bestBid, bestAsk, bidDepth, askDepth decimal.Decimal, ownBuySize, ownSellSize decimal.Decimal, p *Proposal) {
	if pb.orderLevels > 1 {
		return
	}
	// Decibel-specific: we don't have direct order price quantum API here,
	// so we use a simple optimization logic.
	if len(p.Buys) == 1 && !bestBid.IsZero() {
		// Place price slightly better than best bid (e.g., +0.01%)
		improved := bestBid.Mul(decimal.NewFromFloat(1.0001))
		if improved.LessThan(p.Buys[0].Price) {
			p.Buys[0].Price = quantize(improved, pb.priceDecimals)
		}
	}
	if len(p.Sells) == 1 && !bestAsk.IsZero() {
		improved := bestAsk.Mul(decimal.NewFromFloat(0.9999))
		if improved.GreaterThan(p.Sells[0].Price) {
			p.Sells[0].Price = quantize(improved, pb.priceDecimals)
		}
	}
}

// WithinTolerance checks if current and proposal prices are within tolerance
func (pb *ProposalBuilder) WithinTolerance(current []decimal.Decimal, proposal []decimal.Decimal) bool {
	if len(current) != len(proposal) {
		return false
	}
	if len(current) == 0 {
		return true
	}
	tol := pb.bidSpread // reuse as a proxy; in full impl this would come from config
	for i := range current {
		if current[i].IsZero() {
			return false
		}
		diff := proposal[i].Sub(current[i]).Abs().Div(current[i])
		if diff.GreaterThan(tol) {
			return false
		}
	}
	return true
}

func quantize(d decimal.Decimal, places int32) decimal.Decimal {
	scale := decimal.NewFromInt(1).Shift(-places)
	return d.Div(scale).Truncate(0).Mul(scale)
}

// ToBulkOrders converts a proposal to Decibel bulk order bid and ask slices.
func ToBulkOrders(marketAddr string, p *Proposal, tif decibel.TimeInForce, isReduceOnly bool) (bids, asks []decibel.BulkOrderRequest) {
	bids = make([]decibel.BulkOrderRequest, 0, len(p.Buys))
	for _, b := range p.Buys {
		bids = append(bids, decibel.BulkOrderRequest{
			MarketAddr:   marketAddr,
			Price:        b.Price,
			Size:         b.Size,
			IsBuy:        true,
			TimeInForce:  tif,
			IsReduceOnly: isReduceOnly,
		})
	}
	asks = make([]decibel.BulkOrderRequest, 0, len(p.Sells))
	for _, s := range p.Sells {
		asks = append(asks, decibel.BulkOrderRequest{
			MarketAddr:   marketAddr,
			Price:        s.Price,
			Size:         s.Size,
			IsBuy:        false,
			TimeInForce:  tif,
			IsReduceOnly: isReduceOnly,
		})
	}
	return bids, asks
}
