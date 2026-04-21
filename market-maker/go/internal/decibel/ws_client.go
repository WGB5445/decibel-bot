package decibel

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"sync"
	"time"

	"decibel-mm-bot/internal/models"
	"github.com/gorilla/websocket"
	"go.uber.org/zap"
)

// WSClient manages WebSocket connection to Decibel
type WSClient struct {
	url         string
	bearerToken string
	dialer      websocket.Dialer
	conn        *websocket.Conn
	mu          sync.RWMutex
	logger      *zap.Logger
	eventCh     chan models.Event
	stopCh      chan struct{}
	channels    []string
}

// NewWSClient creates a new WebSocket client
func NewWSClient(url, bearerToken string, logger *zap.Logger) *WSClient {
	return &WSClient{
		url:         url,
		bearerToken: bearerToken,
		dialer:      websocket.Dialer{HandshakeTimeout: 10 * time.Second},
		logger:      logger,
		eventCh:     make(chan models.Event, 256),
		stopCh:      make(chan struct{}),
		channels:    make([]string, 0),
	}
}

// SubscribeChannel registers a channel to subscribe after connect
func (w *WSClient) SubscribeChannel(channel string) {
	w.channels = append(w.channels, channel)
}

// Events returns the event output channel
func (w *WSClient) Events() <-chan models.Event {
	return w.eventCh
}

// Start connects and runs the read loop with auto-reconnect
func (w *WSClient) Start(ctx context.Context) {
	for {
		select {
		case <-ctx.Done():
			return
		case <-w.stopCh:
			return
		default:
		}

		if err := w.connectAndRead(ctx); err != nil {
			w.logger.Warn("websocket error, reconnecting", zap.Error(err))
			select {
			case <-ctx.Done():
				return
			case <-w.stopCh:
				return
			case <-time.After(5 * time.Second):
			}
		}
	}
}

// Stop disconnects the websocket
func (w *WSClient) Stop() {
	close(w.stopCh)
	w.mu.Lock()
	if w.conn != nil {
		w.conn.Close()
	}
	w.mu.Unlock()
}

func (w *WSClient) connectAndRead(ctx context.Context) error {
	headers := http.Header{}
	headers.Set("Origin", "https://app.decibel.trade/trade")
	// If bearer auth is required during handshake, some servers accept it in Sec-WebSocket-Protocol
	// or as query param. Decibel docs say WS requires auth; typical pattern is sending auth message post-connect.

	conn, _, err := w.dialer.Dial(w.url, headers)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}

	w.mu.Lock()
	w.conn = conn
	w.mu.Unlock()

	// Send auth message
	authMsg := WSAuthMessage{Type: "auth", Token: w.bearerToken}
	if err := conn.WriteJSON(authMsg); err != nil {
		conn.Close()
		return fmt.Errorf("auth: %w", err)
	}

	// Subscribe to channels
	for _, ch := range w.channels {
		sub := WSSubscribeMessage{Type: "subscribe", Channel: ch}
		if err := conn.WriteJSON(sub); err != nil {
			conn.Close()
			return fmt.Errorf("subscribe %s: %w", ch, err)
		}
	}

	w.logger.Info("websocket connected and subscribed", zap.Int("channels", len(w.channels)))

	for {
		select {
		case <-ctx.Done():
			conn.Close()
			return ctx.Err()
		case <-w.stopCh:
			conn.Close()
			return nil
		default:
		}

		conn.SetReadDeadline(time.Now().Add(30 * time.Second))
		msgType, data, err := conn.ReadMessage()
		if err != nil {
			conn.Close()
			return err
		}
		if msgType != websocket.TextMessage {
			continue
		}

		w.handleMessage(data)
	}
}

func (w *WSClient) handleMessage(data []byte) {
	// Decibel WS messages are JSON with a type/channel field.
	// We'll do a loose parse to detect the message type.
	var envelope map[string]json.RawMessage
	if err := json.Unmarshal(data, &envelope); err != nil {
		w.logger.Debug("failed to unmarshal ws message", zap.Error(err))
		return
	}

	// Try to infer channel from envelope fields
	channel := w.inferChannel(envelope)

	switch channel {
	case "depth":
		w.parseDepth(envelope)
	case "market_price":
		w.parseMarketPrice(envelope)
	case "order_updates":
		w.parseOrderUpdate(envelope)
	case "account_positions":
		w.parsePositionUpdate(envelope)
	case "user_trades":
		w.parseTradeFill(envelope)
	default:
		// Unhandled or unknown message type
	}
}

func (w *WSClient) inferChannel(envelope map[string]json.RawMessage) string {
	// Try explicit channel field
	if chRaw, ok := envelope["channel"]; ok {
		var ch string
		if err := json.Unmarshal(chRaw, &ch); err == nil {
			return ch
		}
	}
	// Try type field
	if typeRaw, ok := envelope["type"]; ok {
		var t string
		if err := json.Unmarshal(typeRaw, &t); err == nil {
			return t
		}
	}
	// Infer from payload shape
	if _, ok := envelope["bids"]; ok {
		return "depth"
	}
	if _, ok := envelope["oracle_px"]; ok {
		return "market_price"
	}
	if _, ok := envelope["order_id"]; ok {
		return "order_updates"
	}
	if _, ok := envelope["position_size"]; ok {
		return "account_positions"
	}
	return ""
}

func (w *WSClient) parseDepth(envelope map[string]json.RawMessage) {
	var market string
	if m, ok := envelope["market"]; ok {
		json.Unmarshal(m, &market)
	}

	bids := parseDepthLevels(envelope["bids"])
	asks := parseDepthLevels(envelope["asks"])

	ev := models.Event{
		Type: models.EventDepthUpdate,
		Data: models.DepthUpdate{
			MarketAddr: market,
			Bids:       bids,
			Asks:       asks,
		},
		TS: time.Now(),
	}
	select {
	case w.eventCh <- ev:
	default:
	}
}

func (w *WSClient) parseMarketPrice(envelope map[string]json.RawMessage) {
	var market string
	if m, ok := envelope["market"]; ok {
		json.Unmarshal(m, &market)
	}

	var mp models.PriceUpdate
	mp.MarketAddr = market
	if v, ok := envelope["oracle_px"]; ok {
		var f float64
		json.Unmarshal(v, &f)
		mp.OraclePx = fmt.Sprintf("%f", f)
	}
	if v, ok := envelope["mark_px"]; ok {
		var f float64
		json.Unmarshal(v, &f)
		mp.MarkPx = fmt.Sprintf("%f", f)
	}
	if v, ok := envelope["mid_px"]; ok {
		var f float64
		json.Unmarshal(v, &f)
		mp.MidPx = fmt.Sprintf("%f", f)
	}
	if v, ok := envelope["funding_rate_bps"]; ok {
		json.Unmarshal(v, &mp.FundingRateBps)
	}
	if v, ok := envelope["is_funding_positive"]; ok {
		json.Unmarshal(v, &mp.IsFundingPositive)
	}

	w.eventCh <- models.Event{Type: models.EventPriceUpdate, Data: mp, TS: time.Now()}
}

func (w *WSClient) parseOrderUpdate(envelope map[string]json.RawMessage) {
	var ou models.OrderUpdate
	json.Unmarshal(envelope["subaccount"], &ou.SubaccountAddr)
	json.Unmarshal(envelope["order_id"], &ou.OrderID)
	json.Unmarshal(envelope["status"], &ou.Status)
	json.Unmarshal(envelope["filled_size"], &ou.FilledSize)
	json.Unmarshal(envelope["avg_price"], &ou.AvgPrice)
	json.Unmarshal(envelope["remaining_size"], &ou.RemainingSize)

	w.eventCh <- models.Event{Type: models.EventOrderUpdate, Data: ou, TS: time.Now()}
}

func (w *WSClient) parsePositionUpdate(envelope map[string]json.RawMessage) {
	var pu models.PositionUpdate
	json.Unmarshal(envelope["subaccount"], &pu.SubaccountAddr)
	json.Unmarshal(envelope["market"], &pu.MarketAddr)
	json.Unmarshal(envelope["position_size"], &pu.PositionAmt)
	json.Unmarshal(envelope["entry_price"], &pu.EntryPrice)
	json.Unmarshal(envelope["leverage"], &pu.Leverage)
	json.Unmarshal(envelope["margin_type"], &pu.MarginType)
	json.Unmarshal(envelope["unrealized_pnl"], &pu.UnrealizedPnl)

	w.eventCh <- models.Event{Type: models.EventPositionUpdate, Data: pu, TS: time.Now()}
}

func (w *WSClient) parseTradeFill(envelope map[string]json.RawMessage) {
	var tf models.TradeFill
	json.Unmarshal(envelope["subaccount"], &tf.SubaccountAddr)
	json.Unmarshal(envelope["market"], &tf.MarketAddr)
	json.Unmarshal(envelope["order_id"], &tf.OrderID)
	json.Unmarshal(envelope["price"], &tf.Price)
	json.Unmarshal(envelope["size"], &tf.Size)

	// Handle side as string (e.g. "buy" / "sell")
	var sideStr string
	if raw, ok := envelope["side"]; ok {
		json.Unmarshal(raw, &sideStr)
	}
	tf.IsBuy = sideStr == "buy" || sideStr == "BUY"

	w.eventCh <- models.Event{Type: models.EventTradeFill, Data: tf, TS: time.Now()}
}

func parseDepthLevels(raw json.RawMessage) []models.PriceLevel {
	if raw == nil {
		return nil
	}
	var levels []DepthLevel
	if err := json.Unmarshal(raw, &levels); err != nil {
		return nil
	}
	out := make([]models.PriceLevel, len(levels))
	for i, l := range levels {
		out[i] = models.PriceLevel{Price: l.Price, Amount: l.Amount}
	}
	return out
}
