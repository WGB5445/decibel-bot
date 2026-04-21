package engine

import (
	"sync"
	"time"

	"decibel-mm-bot/internal/models"
	"github.com/shopspring/decimal"
	"go.uber.org/zap"
)

// PositionManager tracks local position shadow state
type PositionManager struct {
	positions map[string]*models.LocalPosition // key: marketAddr
	mu        sync.RWMutex
	logger    *zap.Logger
}

// NewPositionManager creates a new position manager
func NewPositionManager(logger *zap.Logger) *PositionManager {
	return &PositionManager{
		positions: make(map[string]*models.LocalPosition),
		logger:    logger,
	}
}

// UpdateFromWS updates a position from websocket data
func (pm *PositionManager) UpdateFromWS(update models.PositionUpdate) {
	pm.mu.Lock()
	defer pm.mu.Unlock()

	amt, err := decimal.NewFromString(update.PositionAmt)
	if err != nil {
		pm.logger.Warn("invalid position amt", zap.String("val", update.PositionAmt))
		return
	}
	entry, _ := decimal.NewFromString(update.EntryPrice)
	upnl, _ := decimal.NewFromString(update.UnrealizedPnl)

	if amt.IsZero() {
		delete(pm.positions, update.MarketAddr)
		pm.logger.Info("position closed",
			zap.String("market", update.MarketAddr),
		)
		return
	}

	pm.positions[update.MarketAddr] = &models.LocalPosition{
		MarketAddr:    update.MarketAddr,
		PositionAmt:   amt,
		EntryPrice:    entry,
		Leverage:      update.Leverage,
		MarginType:    update.MarginType,
		UnrealizedPnl: upnl,
		UpdatedAt:     time.Now(),
	}
}

// UpdateFromTradeFill updates position based on a trade fill event
func (pm *PositionManager) UpdateFromTradeFill(fill models.TradeFill) {
	pm.mu.Lock()
	defer pm.mu.Unlock()

	price, err := decimal.NewFromString(fill.Price)
	if err != nil {
		return
	}
	size, err := decimal.NewFromString(fill.Size)
	if err != nil {
		return
	}

	pos, ok := pm.positions[fill.MarketAddr]
	if !ok {
		// New position
		sign := decimal.NewFromInt(1)
		if !fill.IsBuy {
			sign = decimal.NewFromInt(-1)
		}
		pm.positions[fill.MarketAddr] = &models.LocalPosition{
			MarketAddr:  fill.MarketAddr,
			PositionAmt: size.Mul(sign),
			EntryPrice:  price,
			UpdatedAt:   time.Now(),
		}
		return
	}

	// Update existing position
	fillSignedSize := size
	if !fill.IsBuy {
		fillSignedSize = size.Neg()
	}

	newAmt := pos.PositionAmt.Add(fillSignedSize)
	if newAmt.IsZero() {
		delete(pm.positions, fill.MarketAddr)
		return
	}

	// Update entry price as weighted average if adding to position
	if pos.PositionAmt.Sign() == newAmt.Sign() {
		oldNotional := pos.PositionAmt.Abs().Mul(pos.EntryPrice)
		newNotional := fillSignedSize.Abs().Mul(price)
		totalAmt := newAmt.Abs()
		pos.EntryPrice = oldNotional.Add(newNotional).Div(totalAmt)
	}
	pos.PositionAmt = newAmt
	pos.UpdatedAt = time.Now()
}

// GetPosition returns the position for a market
func (pm *PositionManager) GetPosition(marketAddr string) *models.LocalPosition {
	pm.mu.RLock()
	defer pm.mu.RUnlock()
	return pm.positions[marketAddr]
}

// HasPosition returns true if there is a non-zero position for the market
func (pm *PositionManager) HasPosition(marketAddr string) bool {
	pm.mu.RLock()
	defer pm.mu.RUnlock()
	pos, ok := pm.positions[marketAddr]
	return ok && !pos.PositionAmt.IsZero()
}

// AllPositions returns all tracked positions
func (pm *PositionManager) AllPositions() []*models.LocalPosition {
	pm.mu.RLock()
	defer pm.mu.RUnlock()
	out := make([]*models.LocalPosition, 0, len(pm.positions))
	for _, p := range pm.positions {
		out = append(out, p)
	}
	return out
}

// NetExposure returns the sum of absolute position sizes
func (pm *PositionManager) NetExposure() decimal.Decimal {
	pm.mu.RLock()
	defer pm.mu.RUnlock()
	total := decimal.Zero
	for _, p := range pm.positions {
		total = total.Add(p.PositionAmt.Abs())
	}
	return total
}
