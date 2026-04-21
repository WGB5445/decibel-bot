package metrics

import (
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
)

var (
	// Strategy metrics
	ActiveOrders = promauto.NewGaugeVec(prometheus.GaugeOpts{
		Name: "decibel_bot_active_orders",
		Help: "Number of active orders by side",
	}, []string{"side"})

	PositionSize = promauto.NewGauge(prometheus.GaugeOpts{
		Name: "decibel_bot_position_size",
		Help: "Current position size (positive=long, negative=short)",
	})

	UnrealizedPnL = promauto.NewGauge(prometheus.GaugeOpts{
		Name: "decibel_bot_unrealized_pnl",
		Help: "Unrealized PnL in quote asset",
	})

	ReferencePrice = promauto.NewGauge(prometheus.GaugeOpts{
		Name: "decibel_bot_reference_price",
		Help: "Current reference price used for quoting",
	})

	// Transaction metrics
	TxSubmitted = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "decibel_bot_tx_submitted_total",
		Help: "Total on-chain transactions submitted",
	}, []string{"kind"})

	TxConfirmed = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "decibel_bot_tx_confirmed_total",
		Help: "Total on-chain transactions confirmed",
	}, []string{"kind", "status"})

	TxPending = promauto.NewGauge(prometheus.GaugeOpts{
		Name: "decibel_bot_tx_pending",
		Help: "Number of pending transactions",
	})

	// WS metrics
	WSDisconnections = promauto.NewCounter(prometheus.CounterOpts{
		Name: "decibel_bot_ws_disconnections_total",
		Help: "Total WebSocket disconnections",
	})

	WSEventsReceived = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "decibel_bot_ws_events_received_total",
		Help: "Total WebSocket events received",
	}, []string{"event_type"})
)
