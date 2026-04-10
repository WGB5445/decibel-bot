// Package botstate provides the thread-safe shared state between the
// market-maker strategy and the notification layer (e.g. Telegram bot).
// It lives in its own package to avoid import cycles.
package botstate

import (
	"math"
	"strings"
	"sync"
	"time"
)

// Position is a lightweight position type used within botstate.
// Strategy layer is responsible for converting exchange.Position to this type.
type Position struct {
	MarketID string
	Size     float64 // positive = long, negative = short
}

// BotState is written by the market-maker strategy and read by notification
// goroutines. All access to mutable fields must go through Update / Get.
type BotState struct {
	mu sync.RWMutex

	equity       float64
	marginUsage  float64
	inventory    float64
	mid          *float64
	allPositions []Position

	targetMarketID   string
	targetMarketName string

	entryPrices map[string]float64 // normalised market ID -> estimated VWAP entry
	lastCycleAt time.Time
}

// Snapshot is a value-copy of BotState, safe to read without holding the lock.
type Snapshot struct {
	Equity           float64
	MarginUsage      float64
	Inventory        float64
	Mid              *float64
	AllPositions     []Position
	EntryPrice       float64 // VWAP estimate for the target market; 0 = unknown
	TargetMarketName string
	TargetMarketID   string
	LastCycleAt      time.Time
}

// StateUpdate contains the values the strategy pushes each cycle.
type StateUpdate struct {
	Equity        float64
	MarginUsage   float64
	Inventory     float64
	Mid           *float64
	AllPositions  []Position
	PrevInventory float64 // inventory from the PREVIOUS cycle (for VWAP estimator)
}

// New creates a zeroed BotState for the given target market.
func New(targetMarketID, targetMarketName string) *BotState {
	return &BotState{
		targetMarketID:   targetMarketID,
		targetMarketName: targetMarketName,
		entryPrices:      make(map[string]float64),
	}
}

// Update is called by the strategy after each cycle's state fetch.
func (s *BotState) Update(u StateUpdate) {
	s.mu.Lock()
	defer s.mu.Unlock()

	s.equity = u.Equity
	s.marginUsage = u.MarginUsage
	s.inventory = u.Inventory
	s.mid = u.Mid
	s.allPositions = u.AllPositions
	s.lastCycleAt = time.Now()

	key := normalizeID(s.targetMarketID)
	s.entryPrices[key] = estimateEntryPrice(
		s.entryPrices[key], u.PrevInventory, u.Inventory, u.Mid,
	)
}

// Get returns a consistent, lock-free copy of the current state.
func (s *BotState) Get() Snapshot {
	s.mu.RLock()
	defer s.mu.RUnlock()

	key := normalizeID(s.targetMarketID)

	var midCopy *float64
	if s.mid != nil {
		v := *s.mid
		midCopy = &v
	}

	return Snapshot{
		Equity:           s.equity,
		MarginUsage:      s.marginUsage,
		Inventory:        s.inventory,
		Mid:              midCopy,
		AllPositions:     append([]Position(nil), s.allPositions...),
		EntryPrice:       s.entryPrices[key],
		TargetMarketName: s.targetMarketName,
		TargetMarketID:   s.targetMarketID,
		LastCycleAt:      s.lastCycleAt,
	}
}

// IDEqual compares two market IDs using the same normalization.
func IDEqual(a, b string) bool {
	return normalizeID(a) == normalizeID(b)
}

// normalizeID strips "0x" prefix, lowercases, and removes leading zeros.
// Works for Aptos addresses and is a no-op for non-hex IDs.
func normalizeID(id string) string {
	s := strings.TrimPrefix(strings.ToLower(id), "0x")
	s = strings.TrimLeft(s, "0")
	if s == "" {
		return "0"
	}
	return s
}

// estimateEntryPrice updates a VWAP entry-price estimate when a fill is detected.
func estimateEntryPrice(prevEntry, prevSize, newSize float64, mid *float64) float64 {
	if math.Abs(newSize) < 1e-9 {
		return 0 // position closed
	}
	delta := newSize - prevSize
	if math.Abs(delta) < 1e-9 {
		return prevEntry // no fill this cycle
	}
	if mid == nil {
		return prevEntry // can't update without a price
	}
	if prevEntry == 0 || math.Abs(prevSize) < 1e-9 {
		return *mid // brand-new position
	}
	return (prevEntry*math.Abs(prevSize) + (*mid)*math.Abs(delta)) / math.Abs(newSize)
}
