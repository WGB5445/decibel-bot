package strategy

import (
	"context"
	"fmt"
	"time"

	"decibel-mm-bot/internal/config"
	"decibel-mm-bot/internal/decibel"
	"decibel-mm-bot/internal/engine"
	"decibel-mm-bot/internal/models"
	"github.com/shopspring/decimal"
	"go.uber.org/zap"
)

// StrategyState is the state machine state
type StrategyState int

const (
	StateInit StrategyState = iota
	StateNoPosition
	StateMaking
	StatePositionManage
	StateCooldown
)

// DecibelMM is the perpetual market making strategy
type DecibelMM struct {
	cfg              *config.Config
	state            StrategyState
	pricing          *Pricing
	proposalBuilder  *ProposalBuilder
	barrier          *TripleBarrier
	marketAddr       string

	readClient       *decibel.ReadClient
	writeClient      *decibel.WriteClient
	orderMgr         *engine.OrderManager
	positionMgr      *engine.PositionManager
	bus              *engine.EventBus
	logger           *zap.Logger

	cooldownEnd      time.Time
	positionOpenTime time.Time
	lastTxHash       string
	tickCount        int
}

// NewDecibelMM creates the strategy
func NewDecibelMM(
	cfg *config.Config,
	marketAddr string,
	readClient *decibel.ReadClient,
	writeClient *decibel.WriteClient,
	orderMgr *engine.OrderManager,
	positionMgr *engine.PositionManager,
	bus *engine.EventBus,
	logger *zap.Logger,
) *DecibelMM {
	pb := NewProposalBuilder(
		cfg.Strategy.BidSpread,
		cfg.Strategy.AskSpread,
		cfg.Strategy.OrderAmount,
		cfg.Strategy.OrderLevels,
		cfg.Strategy.OrderLevelSpread,
		cfg.Strategy.OrderLevelAmount,
		cfg.Strategy.PriceCeiling,
		cfg.Strategy.PriceFloor,
		cfg.Strategy.MinimumSpread,
		int32(decibel.PriceDecimals),
		int32(decibel.SizeDecimals),
	)

	barrier := NewTripleBarrier(
		cfg.Strategy.StopLoss,
		cfg.Strategy.TakeProfit,
		cfg.Strategy.TimeLimit,
		cfg.Strategy.TrailingStop,
	)

	return &DecibelMM{
		cfg:             cfg,
		state:           StateInit,
		pricing:         NewPricing(),
		proposalBuilder: pb,
		barrier:         barrier,
		marketAddr:      marketAddr,
		readClient:      readClient,
		writeClient:     writeClient,
		orderMgr:        orderMgr,
		positionMgr:     positionMgr,
		bus:             bus,
		logger:          logger,
	}
}

// HandleEvent processes engine events
func (s *DecibelMM) HandleEvent(ev models.Event) {
	switch ev.Type {
	case models.EventDepthUpdate:
		if d, ok := ev.Data.(models.DepthUpdate); ok {
			s.handleDepthUpdate(d)
		}
	case models.EventPriceUpdate:
		if p, ok := ev.Data.(models.PriceUpdate); ok {
			s.handlePriceUpdate(p)
		}
	case models.EventOrderUpdate:
		if o, ok := ev.Data.(models.OrderUpdate); ok {
			s.orderMgr.UpdateFromWS(o)
		}
	case models.EventPositionUpdate:
		if p, ok := ev.Data.(models.PositionUpdate); ok {
			s.positionMgr.UpdateFromWS(p)
		}
	case models.EventTradeFill:
		if t, ok := ev.Data.(models.TradeFill); ok {
			s.handleTradeFill(t)
		}
	case models.EventTxConfirmed:
		if tx, ok := ev.Data.(models.TxResult); ok {
			s.handleTxConfirmed(tx)
		}
	case models.EventTxFailed:
		if tx, ok := ev.Data.(models.TxResult); ok {
			s.logger.Error("tx failed", zap.String("hash", tx.Hash), zap.String("error", tx.Error))
		}
	case models.EventTick:
		s.OnTick(context.Background())
	}
}

func (s *DecibelMM) handleDepthUpdate(d models.DepthUpdate) {
	if len(d.Bids) == 0 || len(d.Asks) == 0 {
		return
	}
	bestBid, _ := decimal.NewFromString(d.Bids[0].Price)
	bestAsk, _ := decimal.NewFromString(d.Asks[0].Price)
	s.pricing.UpdateDepth(bestBid, bestAsk)
}

func (s *DecibelMM) handlePriceUpdate(p models.PriceUpdate) {
	mid, _ := decimal.NewFromString(p.MidPx)
	oracle, _ := decimal.NewFromString(p.OraclePx)
	mark, _ := decimal.NewFromString(p.MarkPx)
	s.pricing.UpdateMarketPrice(mid, oracle, mark, decimal.Zero)
}

func (s *DecibelMM) handleTradeFill(t models.TradeFill) {
	s.positionMgr.UpdateFromTradeFill(t)
	// Enter cooldown on fill
	s.cooldownEnd = time.Now().Add(s.cfg.Strategy.FilledOrderDelay)
	if s.state != StateCooldown {
		s.state = StateCooldown
	}
	if s.positionMgr.HasPosition(s.marketAddr) {
		s.barrier.RecordOpen(time.Now())
		s.positionOpenTime = time.Now()
	}
}

func (s *DecibelMM) handleTxConfirmed(tx models.TxResult) {
	// If bulk order confirmed, we will rely on WS order_updates to update OrderMgr
	// For simple tracking, we can log it here.
	s.logger.Info("tx confirmed", zap.String("hash", tx.Hash), zap.String("kind", tx.Kind))
}

// OnTick is the main strategy loop triggered by scheduler
func (s *DecibelMM) OnTick(ctx context.Context) {
	s.tickCount++
	refPrice := s.pricing.MidPrice()
	if refPrice.IsZero() {
		s.logger.Debug("mid price not available yet, skipping tick")
		return
	}

	if s.tickCount%6 == 0 {
		s.logStatus(refPrice)
	}

	switch s.state {
	case StateInit:
		s.logger.Info("strategy initializing")
		if err := s.setupMarketSettings(ctx); err != nil {
			s.logger.Error("failed to setup market settings", zap.Error(err))
		}
		s.state = StateNoPosition

	case StateCooldown:
		if time.Now().After(s.cooldownEnd) {
			s.logger.Info("cooldown ended")
			if s.positionMgr.HasPosition(s.marketAddr) {
				s.state = StatePositionManage
			} else {
				s.state = StateNoPosition
			}
		}

	case StateNoPosition, StateMaking:
		proposal := s.proposalBuilder.CreateBaseProposal(refPrice)
		s.proposalBuilder.ApplyPriceBand(refPrice, proposal)
		if s.cfg.Strategy.OrderOptimizationEnabled {
			s.proposalBuilder.ApplyOrderOptimization(
				s.pricing.BestBid(), s.pricing.BestAsk(),
				s.cfg.Strategy.BidOrderOptimizationDepth,
				s.cfg.Strategy.AskOrderOptimizationDepth,
				decimal.Zero, decimal.Zero,
				proposal,
			)
		}
		s.proposalBuilder.FilterOutTakers(s.pricing.BestBid(), s.pricing.BestAsk(), proposal)

		// Cancel orders below min spread
		s.cancelOrdersBelowMinSpread(ctx, refPrice)

		if s.cfg.Strategy.UseBulkOrders {
			tif := decibel.TIFGoodTillCanceled
			if s.cfg.Strategy.PostOnly {
				tif = decibel.TIFPostOnly
			}
			bids, asks := ToBulkOrders(s.marketAddr, proposal, tif, false)
			s.submitBulkOrders(ctx, bids, asks)
		} else {
			s.cancelStaleOrders(ctx, proposal)
			if s.shouldCreateOrders() {
				s.executeProposal(ctx, proposal)
			}
		}

		if s.positionMgr.HasPosition(s.marketAddr) {
			s.state = StatePositionManage
		} else {
			s.state = StateMaking
		}

	case StatePositionManage:
		// Profit taking
		tpProposal := s.profitTakingProposal()
		if tpProposal != nil {
			if s.cfg.Strategy.UseBulkOrders {
				// Careful: bulk order would overwrite market making orders.
				// For now, use single place order for TP/SL in position manage state.
				s.executeProposal(ctx, tpProposal)
			} else {
				s.executeProposal(ctx, tpProposal)
			}
		}

		// Stop loss / time limit / trailing stop
		pos := s.positionMgr.GetPosition(s.marketAddr)
		if pos != nil {
			pnlPct := ComputePnLPct(pos, refPrice)
			if s.barrier.CheckStopLoss(pnlPct) || s.barrier.CheckTrailingStop(pnlPct) {
				s.logger.Warn("stop loss triggered", zap.String("pnl_pct", pnlPct.String()))
				s.closeAllWithMarket(ctx)
				s.state = StateCooldown
				return
			}
			if s.barrier.CheckTakeProfit(pnlPct) {
				s.logger.Info("take profit triggered", zap.String("pnl_pct", pnlPct.String()))
				s.closeAllWithMarket(ctx)
				s.state = StateCooldown
				return
			}
			if s.barrier.CheckTimeLimit(time.Now()) {
				s.logger.Info("time limit exceeded, closing position")
				s.closeAllWithMarket(ctx)
				s.state = StateCooldown
				return
			}
		}

		if !s.positionMgr.HasPosition(s.marketAddr) {
			s.state = StateNoPosition
		}
	}
}

func (s *DecibelMM) submitBulkOrders(ctx context.Context, bids, asks []decibel.BulkOrderRequest) {
	if len(bids) == 0 && len(asks) == 0 {
		return
	}
	s.logger.Info("submitting bulk orders", zap.Int("bids", len(bids)), zap.Int("asks", len(asks)))
	result, err := s.writeClient.PlaceBulkOrders(ctx, s.readClient, s.cfg.Decibel.SubaccountAddr, s.marketAddr, bids, asks)
	if err != nil {
		s.logger.Error("failed to submit bulk orders", zap.Error(err))
	} else {
		s.lastTxHash = result.Hash
		// Replace local shadow orders for this market with pending bulk orders
		orders := append(bids, asks...)
		newOrders := make([]*models.LocalOrder, len(orders))
		for i, o := range orders {
			side := models.SideSell
			if o.IsBuy {
				side = models.SideBuy
			}
			newOrders[i] = &models.LocalOrder{
				ClientOrderID: result.Hash + "-" + fmt.Sprintf("%d", i),
				MarketAddr:    o.MarketAddr,
				Side:          side,
				Price:         o.Price,
				Size:          o.Size,
				Status:        models.OrderStatusPendingNew,
				IsReduceOnly:  o.IsReduceOnly,
				TxHash:        result.Hash,
			}
		}
		s.orderMgr.ReplaceAllForMarket(s.marketAddr, newOrders)
	}
}

func (s *DecibelMM) executeProposal(ctx context.Context, p *Proposal) {
	for _, buy := range p.Buys {
		req := decibel.PlaceOrderRequest{
			PackageAddr:    decibel.PackageAddrForNetwork(s.cfg.Env),
			SubaccountAddr: s.cfg.Decibel.SubaccountAddr,
			MarketAddr:     s.marketAddr,
			Price:          buy.Price,
			Size:           buy.Size,
			IsBuy:          true,
			TimeInForce:    s.getTimeInForce(),
			IsReduceOnly:   false,
		}
		result, err := s.writeClient.PlaceOrder(ctx, req)
		if err != nil {
			s.logger.Error("failed to place buy order", zap.Error(err))
		} else {
			outcome := decibel.OutcomeFromTxResult(result)
			s.orderMgr.AddPending(outcome.TxHash, outcome.TxHash, models.SideBuy, buy.Price, buy.Size, false)
			if lo := s.orderMgr.GetOrder(outcome.TxHash); lo != nil {
				lo.MarketAddr = s.marketAddr
				lo.OrderID = outcome.OrderID
			}
			s.lastTxHash = outcome.TxHash
		}
	}
	for _, sell := range p.Sells {
		req := decibel.PlaceOrderRequest{
			PackageAddr:    decibel.PackageAddrForNetwork(s.cfg.Env),
			SubaccountAddr: s.cfg.Decibel.SubaccountAddr,
			MarketAddr:     s.marketAddr,
			Price:          sell.Price,
			Size:           sell.Size,
			IsBuy:          false,
			TimeInForce:    s.getTimeInForce(),
			IsReduceOnly:   false,
		}
		result, err := s.writeClient.PlaceOrder(ctx, req)
		if err != nil {
			s.logger.Error("failed to place sell order", zap.Error(err))
		} else {
			outcome := decibel.OutcomeFromTxResult(result)
			s.orderMgr.AddPending(outcome.TxHash, outcome.TxHash, models.SideSell, sell.Price, sell.Size, false)
			if lo := s.orderMgr.GetOrder(outcome.TxHash); lo != nil {
				lo.MarketAddr = s.marketAddr
				lo.OrderID = outcome.OrderID
			}
			s.lastTxHash = outcome.TxHash
		}
	}
}

func (s *DecibelMM) closeAllWithMarket(ctx context.Context) {
	pos := s.positionMgr.GetPosition(s.marketAddr)
	if pos == nil || pos.PositionAmt.IsZero() {
		return
	}
	isBuy := pos.IsShort()
	req := decibel.PlaceOrderRequest{
		PackageAddr:    decibel.PackageAddrForNetwork(s.cfg.Env),
		SubaccountAddr: s.cfg.Decibel.SubaccountAddr,
		MarketAddr:     s.marketAddr,
		Price:          decimal.Zero, // Market order: price can be 0 or use current price
		Size:           pos.PositionAmt.Abs(),
		IsBuy:          isBuy,
		TimeInForce:    decibel.TIFImmediateOrCancel,
		IsReduceOnly:   true,
	}
	result, err := s.writeClient.PlaceOrder(ctx, req)
	if err != nil {
		s.logger.Error("failed to close position", zap.Error(err))
	} else {
		s.logger.Info("closing position", zap.String("hash", result.Hash), zap.String("size", pos.PositionAmt.Abs().String()))
	}
}

func (s *DecibelMM) profitTakingProposal() *Proposal {
	pos := s.positionMgr.GetPosition(s.marketAddr)
	if pos == nil || pos.PositionAmt.IsZero() {
		return nil
	}

	var targetPrice decimal.Decimal
	if pos.IsLong() && !s.cfg.Strategy.LongProfitTakingSpread.IsZero() {
		targetPrice = pos.EntryPrice.Mul(decimal.NewFromInt(1).Add(s.cfg.Strategy.LongProfitTakingSpread))
	} else if pos.IsShort() && !s.cfg.Strategy.ShortProfitTakingSpread.IsZero() {
		targetPrice = pos.EntryPrice.Mul(decimal.NewFromInt(1).Sub(s.cfg.Strategy.ShortProfitTakingSpread))
	} else {
		return nil
	}

	side := models.SideSell
	if pos.IsShort() {
		side = models.SideBuy
	}

	// Check if an existing reduce-only order is already at the target price
	var existing bool
	for _, o := range s.orderMgr.ActiveOrders() {
		if o.IsReduceOnly && o.Side == side && o.Price.Equal(targetPrice) {
			existing = true
			break
		}
	}
	if existing {
		return nil
	}

	return &Proposal{
		Buys:  []PriceSize{{Price: targetPrice, Size: pos.PositionAmt.Abs()}},
		Sells: []PriceSize{{Price: targetPrice, Size: pos.PositionAmt.Abs()}},
	}
}

func (s *DecibelMM) cancelStaleOrders(ctx context.Context, proposal *Proposal) {
	active := s.orderMgr.ActiveOrders()
	if len(active) == 0 {
		return
	}

	currentBuyPrices := make([]decimal.Decimal, 0)
	currentSellPrices := make([]decimal.Decimal, 0)
	for _, o := range active {
		if o.Status != models.OrderStatusNew && o.Status != models.OrderStatusPartiallyFilled {
			continue
		}
		if o.Side == models.SideBuy {
			currentBuyPrices = append(currentBuyPrices, o.Price)
		} else {
			currentSellPrices = append(currentSellPrices, o.Price)
		}
	}

	proposalBuyPrices := make([]decimal.Decimal, len(proposal.Buys))
	for i, b := range proposal.Buys {
		proposalBuyPrices[i] = b.Price
	}
	proposalSellPrices := make([]decimal.Decimal, len(proposal.Sells))
	for i, s := range proposal.Sells {
		proposalSellPrices[i] = s.Price
	}

	deferStale := s.proposalBuilder.WithinTolerance(currentBuyPrices, proposalBuyPrices) &&
		s.proposalBuilder.WithinTolerance(currentSellPrices, proposalSellPrices)

	if deferStale {
		s.logger.Debug("not canceling active orders, within tolerance")
		return
	}

	for _, o := range active {
		if o.Status == models.OrderStatusNew || o.Status == models.OrderStatusPartiallyFilled {
			_, err := s.writeClient.CancelOrder(ctx, s.cfg.Decibel.SubaccountAddr, o.MarketAddr, o.OrderID)
			if err != nil {
				s.logger.Error("failed to cancel order", zap.String("order_id", o.OrderID), zap.Error(err))
			} else {
				s.orderMgr.CancelOrder(o.OrderID)
			}
		}
	}
}

func (s *DecibelMM) cancelOrdersBelowMinSpread(ctx context.Context, refPrice decimal.Decimal) {
	if s.cfg.Strategy.MinimumSpread.LessThanOrEqual(decimal.NewFromInt(-99)) {
		return
	}
	for _, o := range s.orderMgr.ActiveOrders() {
		if o.Status != models.OrderStatusNew && o.Status != models.OrderStatusPartiallyFilled {
			continue
		}
		var spread decimal.Decimal
		if o.Side == models.SideBuy {
			spread = refPrice.Sub(o.Price).Div(refPrice)
		} else {
			spread = o.Price.Sub(refPrice).Div(refPrice)
		}
		if spread.LessThan(s.cfg.Strategy.MinimumSpread) {
			_, err := s.writeClient.CancelOrder(ctx, s.cfg.Decibel.SubaccountAddr, o.MarketAddr, o.OrderID)
			if err != nil {
				s.logger.Error("failed to cancel order below min spread", zap.String("order_id", o.OrderID), zap.Error(err))
			} else {
				s.orderMgr.CancelOrder(o.OrderID)
			}
		}
	}
}

func (s *DecibelMM) shouldCreateOrders() bool {
	return !s.orderMgr.HasOpenOrders()
}

func (s *DecibelMM) getTimeInForce() decibel.TimeInForce {
	if s.cfg.Strategy.PostOnly {
		return decibel.TIFPostOnly
	}
	return decibel.TIFGoodTillCanceled
}

// SetMarketAddr sets the resolved market address
func (s *DecibelMM) SetMarketAddr(addr string) {
	s.marketAddr = addr
}

func (s *DecibelMM) setupMarketSettings(ctx context.Context) error {
	// Decibel uses leverage in basis points (e.g., 1000 = 10x)
	leverageBps := uint64(s.cfg.Strategy.Leverage * 100)
	isCross := true // default to cross margin for market making
	_, err := s.writeClient.ConfigureMarketSettings(ctx, s.cfg.Decibel.SubaccountAddr, s.marketAddr, leverageBps, isCross)
	return err
}

func (s *DecibelMM) logStatus(refPrice decimal.Decimal) {
	pos := s.positionMgr.GetPosition(s.marketAddr)
	active := s.orderMgr.ActiveOrders()
	buys := 0
	sells := 0
	for _, o := range active {
		if o.Side == models.SideBuy {
			buys++
		} else {
			sells++
		}
	}

	if pos != nil && !pos.PositionAmt.IsZero() {
		pnl := ComputePnLPct(pos, refPrice)
		s.logger.Info("strategy status",
			zap.String("state", s.state.String()),
			zap.String("ref_price", refPrice.String()),
			zap.String("position_amt", pos.PositionAmt.String()),
			zap.String("entry_price", pos.EntryPrice.String()),
			zap.String("unrealized_pnl_pct", pnl.String()),
			zap.Int("active_buys", buys),
			zap.Int("active_sells", sells),
		)
	} else {
		s.logger.Info("strategy status",
			zap.String("state", s.state.String()),
			zap.String("ref_price", refPrice.String()),
			zap.Int("active_buys", buys),
			zap.Int("active_sells", sells),
		)
	}
}
