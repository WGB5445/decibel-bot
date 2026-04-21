package engine

import (
	"sync"
	"time"

	"github.com/bujih/decibel-mm-go/internal/models"
	"github.com/shopspring/decimal"
	"go.uber.org/zap"
)

// OrderManager tracks local order shadow state
type OrderManager struct {
	active   map[string]*models.LocalOrder // key: orderID
	history  []*models.LocalOrder
	mu       sync.RWMutex
	logger   *zap.Logger
}

// NewOrderManager creates a new order manager
func NewOrderManager(logger *zap.Logger) *OrderManager {
	return &OrderManager{
		active:  make(map[string]*models.LocalOrder),
		history: make([]*models.LocalOrder, 0),
		logger:  logger,
	}
}

// AddPending records an order that has been submitted but not yet confirmed
func (om *OrderManager) AddPending(clientOrderID, txHash string, side models.Side, price, size decimal.Decimal, isReduceOnly bool) {
	om.mu.Lock()
	defer om.mu.Unlock()
	order := &models.LocalOrder{
		ClientOrderID: clientOrderID,
		Side:          side,
		Price:         price,
		Size:          size,
		Status:        models.OrderStatusPendingNew,
		IsReduceOnly:  isReduceOnly,
		CreatedAt:     time.Now(),
		TxHash:        txHash,
	}
	om.active[clientOrderID] = order
}

// ConfirmOrder upgrades a pending order to active once tx is confirmed
func (om *OrderManager) ConfirmOrder(clientOrderID, orderID string) {
	om.mu.Lock()
	defer om.mu.Unlock()
	order, ok := om.active[clientOrderID]
	if !ok {
		return
	}
	order.OrderID = orderID
	order.Status = models.OrderStatusNew
	order.UpdatedAt = time.Now()
	// Re-key by orderID if known
	if orderID != "" && orderID != clientOrderID {
		delete(om.active, clientOrderID)
		om.active[orderID] = order
	}
}

// UpdateFromWS updates order state from websocket order update
func (om *OrderManager) UpdateFromWS(update models.OrderUpdate) {
	om.mu.Lock()
	defer om.mu.Unlock()

	order := om.findByID(update.OrderID)
	if order == nil {
		return
	}

	filled, _ := decimal.NewFromString(update.FilledSize)
	avgPrice, _ := decimal.NewFromString(update.AvgPrice)
	order.FilledSize = filled
	order.AvgPrice = avgPrice
	order.Status = models.OrderStatus(update.Status)
	order.UpdatedAt = time.Now()

	if order.IsDone() {
		delete(om.active, update.OrderID)
		om.history = append(om.history, order)
		om.logger.Info("order moved to history",
			zap.String("order_id", update.OrderID),
			zap.String("status", string(order.Status)),
		)
	}
}

// CancelOrder marks an order as cancelled
func (om *OrderManager) CancelOrder(orderID string) {
	om.mu.Lock()
	defer om.mu.Unlock()
	order, ok := om.active[orderID]
	if !ok {
		return
	}
	order.Status = models.OrderStatusCancelled
	order.UpdatedAt = time.Now()
	delete(om.active, orderID)
	om.history = append(om.history, order)
}

// ReplaceAllForMarket replaces all active orders for a market (used after bulk order tx)
func (om *OrderManager) ReplaceAllForMarket(marketAddr string, orders []*models.LocalOrder) {
	om.mu.Lock()
	defer om.mu.Unlock()
	// Remove existing active orders for this market
	for id, o := range om.active {
		if o.MarketAddr == marketAddr {
			delete(om.active, id)
		}
	}
	// Add new ones
	for _, o := range orders {
		key := o.OrderID
		if key == "" {
			key = o.ClientOrderID
		}
		om.active[key] = o
	}
}

// ActiveOrders returns all active orders
func (om *OrderManager) ActiveOrders() []*models.LocalOrder {
	om.mu.RLock()
	defer om.mu.RUnlock()
	out := make([]*models.LocalOrder, 0, len(om.active))
	for _, o := range om.active {
		out = append(out, o)
	}
	return out
}

// ActiveBuys returns active buy orders
func (om *OrderManager) ActiveBuys() []*models.LocalOrder {
	out := make([]*models.LocalOrder, 0)
	for _, o := range om.ActiveOrders() {
		if o.Side == models.SideBuy {
			out = append(out, o)
		}
	}
	return out
}

// ActiveSells returns active sell orders
func (om *OrderManager) ActiveSells() []*models.LocalOrder {
	out := make([]*models.LocalOrder, 0)
	for _, o := range om.ActiveOrders() {
		if o.Side == models.SideSell {
			out = append(out, o)
		}
	}
	return out
}

// GetOrder retrieves an active order by ID
func (om *OrderManager) GetOrder(orderID string) *models.LocalOrder {
	om.mu.RLock()
	defer om.mu.RUnlock()
	return om.active[orderID]
}

// HasOpenOrders returns true if there are any active orders
func (om *OrderManager) HasOpenOrders() bool {
	om.mu.RLock()
	defer om.mu.RUnlock()
	return len(om.active) > 0
}

func (om *OrderManager) findByID(orderID string) *models.LocalOrder {
	if o, ok := om.active[orderID]; ok {
		return o
	}
	for _, o := range om.active {
		if o.ClientOrderID == orderID {
			return o
		}
	}
	return nil
}
