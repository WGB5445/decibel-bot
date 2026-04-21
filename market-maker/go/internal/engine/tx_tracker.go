package engine

import (
	"context"
	"sync"
	"time"

	"github.com/aptos-labs/aptos-go-sdk/api"
	"decibel-mm-bot/internal/models"
	"go.uber.org/zap"
)

// TxKind identifies the purpose of a transaction
type TxKind string

const (
	TxKindPlaceOrder  TxKind = "PLACE_ORDER"
	TxKindCancelOrder TxKind = "CANCEL_ORDER"
	TxKindCancelAll   TxKind = "CANCEL_ALL"
	TxKindBulkOrder   TxKind = "BULK_ORDER"
	TxKindDeposit     TxKind = "DEPOSIT"
	TxKindWithdraw    TxKind = "WITHDRAW"
)

// PendingTx tracks a submitted transaction
type PendingTx struct {
	Hash        string
	Kind        TxKind
	SubmittedAt time.Time
	Metadata    interface{}
}

// TxPoller defines how to poll transaction status
type TxPoller interface {
	WaitForTransaction(ctx context.Context, hash string) (*api.UserTransaction, error)
}

// TxTracker tracks pending on-chain transactions and emits events
type TxTracker struct {
	pending    map[string]*PendingTx
	mu         sync.RWMutex
	poller     TxPoller
	bus        *EventBus
	interval   time.Duration
	maxPending int
	logger     *zap.Logger
	stopCh     chan struct{}
}

// NewTxTracker creates a new transaction tracker
func NewTxTracker(poller TxPoller, bus *EventBus, interval time.Duration, maxPending int, logger *zap.Logger) *TxTracker {
	return &TxTracker{
		pending:    make(map[string]*PendingTx),
		poller:     poller,
		bus:        bus,
		interval:   interval,
		maxPending: maxPending,
		logger:     logger,
		stopCh:     make(chan struct{}),
	}
}

// Start begins the background polling loop
func (tt *TxTracker) Start(ctx context.Context) {
	ticker := time.NewTicker(tt.interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-tt.stopCh:
			return
		case <-ticker.C:
			tt.pollPending(ctx)
		}
	}
}

// Stop halts the polling loop
func (tt *TxTracker) Stop() {
	close(tt.stopCh)
}

// Register adds a pending transaction
func (tt *TxTracker) Register(hash string, kind TxKind, metadata interface{}) bool {
	tt.mu.Lock()
	defer tt.mu.Unlock()
	if len(tt.pending) >= tt.maxPending {
		tt.logger.Warn("max pending transactions reached, dropping tx",
			zap.String("hash", hash),
			zap.String("kind", string(kind)),
		)
		return false
	}
	tt.pending[hash] = &PendingTx{
		Hash:        hash,
		Kind:        kind,
		SubmittedAt: time.Now(),
		Metadata:    metadata,
	}
	tt.logger.Info("tx registered", zap.String("hash", hash), zap.String("kind", string(kind)))
	return true
}

// GetPending returns a snapshot of pending transactions
func (tt *TxTracker) GetPending() []*PendingTx {
	tt.mu.RLock()
	defer tt.mu.RUnlock()
	out := make([]*PendingTx, 0, len(tt.pending))
	for _, v := range tt.pending {
		out = append(out, v)
	}
	return out
}

func (tt *TxTracker) pollPending(ctx context.Context) {
	txList := tt.GetPending()
	for _, tx := range txList {
		if time.Since(tx.SubmittedAt) > 60*time.Second {
			tt.logger.Warn("tx timed out", zap.String("hash", tx.Hash))
			tt.remove(tx.Hash)
			tt.bus.Publish(models.Event{
				Type: models.EventTxFailed,
				Data: models.TxResult{
					Hash:    tx.Hash,
					Success: false,
					Error:   "timeout",
					Kind:    string(tx.Kind),
				},
			})
			continue
		}

		utx, err := tt.poller.WaitForTransaction(ctx, tx.Hash)
		if err != nil {
			// Not yet confirmed or network error; skip for now
			continue
		}

		tt.remove(tx.Hash)
		success := utx != nil && utx.Success
		errMsg := ""
		if utx != nil && utx.VmStatus != "" {
			errMsg = utx.VmStatus
		}
		if !success {
			errMsg = "transaction failed"
		}

		if success {
			tt.bus.Publish(models.Event{
				Type: models.EventTxConfirmed,
				Data: models.TxResult{
					Hash:    tx.Hash,
					Success: true,
					Kind:    string(tx.Kind),
				},
			})
		} else {
			tt.bus.Publish(models.Event{
				Type: models.EventTxFailed,
				Data: models.TxResult{
					Hash:    tx.Hash,
					Success: false,
					Error:   errMsg,
					Kind:    string(tx.Kind),
				},
			})
		}
	}
}

func (tt *TxTracker) remove(hash string) {
	tt.mu.Lock()
	defer tt.mu.Unlock()
	delete(tt.pending, hash)
}
