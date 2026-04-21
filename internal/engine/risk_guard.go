package engine

import (
	"github.com/bujih/decibel-mm-go/internal/decibel"
	"github.com/bujih/decibel-mm-go/internal/models"
	"github.com/shopspring/decimal"
	"go.uber.org/zap"
)

// RiskGuard evaluates safety conditions and can halt trading
type RiskGuard struct {
	maxDailyLossUSD   decimal.Decimal
	liquidationBuffer decimal.Decimal // e.g. 0.1 means halt if within 10% of liq price
	logger            *zap.Logger
}

// NewRiskGuard creates a new risk guard
func NewRiskGuard(maxDailyLossUSD, liquidationBuffer decimal.Decimal, logger *zap.Logger) *RiskGuard {
	return &RiskGuard{
		maxDailyLossUSD:   maxDailyLossUSD,
		liquidationBuffer: liquidationBuffer,
		logger:            logger,
	}
}

// Evaluate checks current risk conditions
func (rg *RiskGuard) Evaluate(position *models.LocalPosition, overview *decibel.AccountOverview, markPrice decimal.Decimal) (halt bool, reason string) {
	if position != nil && !position.LiquidationPrice.IsZero() && !markPrice.IsZero() {
		var distance decimal.Decimal
		if position.IsLong() {
			distance = markPrice.Sub(position.LiquidationPrice).Div(markPrice)
		} else {
			distance = position.LiquidationPrice.Sub(markPrice).Div(markPrice)
		}
		if distance.LessThanOrEqual(rg.liquidationBuffer) {
			rg.logger.Warn("approaching liquidation",
				zap.String("distance", distance.String()),
				zap.String("liq_price", position.LiquidationPrice.String()),
				zap.String("mark_price", markPrice.String()),
			)
			return true, "approaching liquidation"
		}
	}

	if overview != nil && !rg.maxDailyLossUSD.IsZero() {
		// Decibel's account_overview doesn't expose daily PnL directly,
		// but we can approximate from unrealized_pnl if we track session start equity elsewhere.
		// For now, this is a placeholder.
	}

	return false, ""
}
