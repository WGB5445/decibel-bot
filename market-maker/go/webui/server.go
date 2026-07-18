// Package webui implements an optional local HTTP dashboard for monitoring and
// controlling the market-maker bot, alongside (not instead of) the Telegram
// notifier. It reuses notify.InfoProvider so both surfaces share one source of
// truth and one set of control actions.
//
// Security: authentication is a single shared-secret token (WEB_UI_TOKEN),
// checked with a constant-time comparison. There is no user/session system.
// Config.Bind defaults to loopback-only; binding to a non-loopback address is
// logged as a loud warning at startup. Do not expose this directly to the
// public internet — put it behind a VPN, SSH tunnel, or authenticating reverse
// proxy if remote access is needed.
package webui

import (
	"context"
	"crypto/subtle"
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"strings"
	"time"

	"decibel-mm-bot/botstate"
	"decibel-mm-bot/notify"
)

//go:embed static/index.html
var staticFS embed.FS

// Config holds Web UI configuration.
type Config struct {
	// Bind is the listen address, e.g. "127.0.0.1:8090".
	Bind string
	// Token is the shared secret required on every request.
	Token string
}

// Server implements a local HTTP dashboard backed by notify.InfoProvider.
type Server struct {
	cfg  Config
	info notify.InfoProvider
}

// New creates a Server. It does not start listening until Run is called.
func New(cfg Config, info notify.InfoProvider) *Server {
	return &Server{cfg: cfg, info: info}
}

// Handler builds the http.Handler for the dashboard (routes + auth middleware).
// Exposed separately from Run so it can be exercised in tests via httptest
// without binding a real listener.
func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/", s.handleIndex)
	mux.HandleFunc("/api/status", s.withAuth(s.handleStatus))
	mux.HandleFunc("/api/positions", s.withAuth(s.handlePositions))
	mux.HandleFunc("/api/pause", s.withAuth(s.handlePause))
	mux.HandleFunc("/api/resume", s.withAuth(s.handleResume))
	mux.HandleFunc("/api/flatten", s.withAuth(s.handleFlatten))
	return mux
}

// Run starts the HTTP server and blocks until ctx is cancelled, then shuts down
// gracefully within a bounded timeout.
func (s *Server) Run(ctx context.Context) error {
	if strings.TrimSpace(s.cfg.Token) == "" {
		return errors.New("webui: Token must not be empty")
	}
	warnIfNotLoopback(s.cfg.Bind)

	httpSrv := &http.Server{
		Addr:              s.cfg.Bind,
		Handler:           s.Handler(),
		ReadHeaderTimeout: 10 * time.Second,
	}

	errCh := make(chan error, 1)
	go func() {
		slog.Info("webui: listening", "addr", s.cfg.Bind)
		if err := httpSrv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			errCh <- err
			return
		}
		errCh <- nil
	}()

	select {
	case <-ctx.Done():
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if err := httpSrv.Shutdown(shutdownCtx); err != nil {
			return fmt.Errorf("webui: shutdown: %w", err)
		}
		return nil
	case err := <-errCh:
		return err
	}
}

// warnIfNotLoopback logs a loud warning when Bind is not restricted to loopback,
// since Web UI auth is a single shared token with no rate limiting or lockout.
func warnIfNotLoopback(bind string) {
	host, _, err := net.SplitHostPort(bind)
	if err != nil {
		host = bind
	}
	switch host {
	case "127.0.0.1", "localhost", "::1", "":
		return
	}
	slog.Warn("webui: bound to a non-loopback address — this exposes pause/resume/flatten controls behind only a shared token; put it behind a VPN, SSH tunnel, or authenticating reverse proxy",
		"bind", bind)
}

// ── Auth ─────────────────────────────────────────────────────────────────────

func (s *Server) withAuth(next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		token := r.URL.Query().Get("token")
		if token == "" {
			if auth := r.Header.Get("Authorization"); strings.HasPrefix(auth, "Bearer ") {
				token = strings.TrimPrefix(auth, "Bearer ")
			}
		}
		if subtle.ConstantTimeCompare([]byte(token), []byte(s.cfg.Token)) != 1 {
			http.Error(w, `{"error":"unauthorized"}`, http.StatusUnauthorized)
			return
		}
		next(w, r)
	}
}

// ── Handlers ─────────────────────────────────────────────────────────────────

func (s *Server) handleIndex(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/" {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	b, err := staticFS.ReadFile("static/index.html")
	if err != nil {
		http.Error(w, "index not found", http.StatusInternalServerError)
		return
	}
	_, _ = w.Write(b)
}

type statusPayload struct {
	MarketName          string   `json:"market_name"`
	Paused              bool     `json:"paused"`
	PauseCancelResting  bool     `json:"pause_cancel_resting"`
	EffectiveSpread     float64  `json:"effective_spread"`
	Inventory           float64  `json:"inventory"`
	MaxInventory        float64  `json:"max_inventory"`
	MarginUsage         float64  `json:"margin_usage"`
	MaxMarginUsage      float64  `json:"max_margin_usage"`
	FlattenStuckSeconds float64  `json:"flatten_stuck_seconds"`
	FlattenForceSeconds float64  `json:"flatten_force_seconds"`
	ForceCloseCount     int      `json:"force_close_count"`
	Equity              float64  `json:"equity"`
	Mid                 *float64 `json:"mid"`
	DryRun              bool     `json:"dry_run"`
	LastCycleAt         string   `json:"last_cycle_at"`
}

func (s *Server) handleStatus(w http.ResponseWriter, r *http.Request) {
	ctx, cancel := context.WithTimeout(r.Context(), 15*time.Second)
	defer cancel()

	snap, err := s.info.FetchLiveSnapshot(ctx)
	if err != nil {
		writeJSONError(w, http.StatusBadGateway, err)
		return
	}

	lastCycle := ""
	if !snap.LastCycleAt.IsZero() {
		lastCycle = snap.LastCycleAt.Format(time.RFC3339)
	}

	writeJSON(w, statusPayload{
		MarketName:          snap.TargetMarketName,
		Paused:              snap.Paused,
		PauseCancelResting:  snap.PauseCancelResting,
		EffectiveSpread:     snap.EffectiveSpread,
		Inventory:           snap.Inventory,
		MaxInventory:        s.info.MaxInventory(),
		MarginUsage:         snap.MarginUsage,
		MaxMarginUsage:      s.info.MaxMarginUsage(),
		FlattenStuckSeconds: snap.FlattenStuckSeconds,
		FlattenForceSeconds: s.info.FlattenForceSeconds(),
		ForceCloseCount:     snap.ForceCloseCount,
		Equity:              snap.Equity,
		Mid:                 snap.Mid,
		DryRun:              s.info.DryRun(),
		LastCycleAt:         lastCycle,
	})
}

type positionPayload struct {
	MarketID                  string  `json:"market_id"`
	MarketName                string  `json:"market_name"`
	Size                      float64 `json:"size"`
	EntryPrice                float64 `json:"entry_price"`
	Mark                      float64 `json:"mark,omitempty"`
	HasMark                   bool    `json:"has_mark"`
	UserLeverage              float64 `json:"user_leverage"`
	UnrealizedFunding         float64 `json:"unrealized_funding"`
	EstimatedLiquidationPrice float64 `json:"estimated_liquidation_price"`
}

func (s *Server) handlePositions(w http.ResponseWriter, r *http.Request) {
	ctx, cancel := context.WithTimeout(r.Context(), 15*time.Second)
	defer cancel()

	snap, err := s.info.FetchLiveSnapshot(ctx)
	if err != nil {
		writeJSONError(w, http.StatusBadGateway, err)
		return
	}

	out := make([]positionPayload, 0, len(snap.AllPositions))
	for _, p := range snap.AllPositions {
		if p.IsDeleted {
			continue
		}
		pp := positionPayload{
			MarketID:                  p.MarketID,
			MarketName:                s.info.MarketDisplayName(p.MarketID),
			Size:                      p.Size,
			EntryPrice:                p.EntryPrice,
			UserLeverage:              p.UserLeverage,
			UnrealizedFunding:         p.UnrealizedFunding,
			EstimatedLiquidationPrice: p.EstimatedLiquidationPrice,
		}
		if botstate.IDEqual(p.MarketID, snap.TargetMarketID) && snap.Mid != nil {
			pp.Mark = *snap.Mid
			pp.HasMark = true
		} else if snap.MidByMarket != nil {
			if mid, ok := snap.MidByMarket[p.MarketID]; ok {
				pp.Mark = mid
				pp.HasMark = true
			}
		}
		out = append(out, pp)
	}
	writeJSON(w, out)
}

type pauseRequest struct {
	CancelResting bool `json:"cancel_resting"`
}

func (s *Server) handlePause(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, `{"error":"method not allowed"}`, http.StatusMethodNotAllowed)
		return
	}
	var req pauseRequest
	if r.Body != nil {
		_ = json.NewDecoder(r.Body).Decode(&req) // empty/absent body = defaults (cancel_resting=false)
	}

	ctx, cancel := context.WithTimeout(r.Context(), 20*time.Second)
	defer cancel()
	if err := s.info.PauseTrading(ctx, req.CancelResting); err != nil {
		writeJSONError(w, http.StatusBadGateway, err)
		return
	}
	writeJSON(w, map[string]any{"ok": true, "paused": true, "cancel_resting": req.CancelResting})
}

func (s *Server) handleResume(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, `{"error":"method not allowed"}`, http.StatusMethodNotAllowed)
		return
	}
	s.info.ResumeTrading()
	writeJSON(w, map[string]any{"ok": true, "paused": false})
}

func (s *Server) handleFlatten(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, `{"error":"method not allowed"}`, http.StatusMethodNotAllowed)
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 30*time.Second)
	defer cancel()
	outcome, err := s.info.FlattenPosition(ctx)
	if err != nil {
		writeJSONError(w, http.StatusBadGateway, err)
		return
	}
	writeJSON(w, map[string]any{"ok": true, "tx_hash": outcome.TxHash, "order_id": outcome.OrderID})
}

// ── JSON helpers ─────────────────────────────────────────────────────────────

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

func writeJSONError(w http.ResponseWriter, status int, err error) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]string{"error": err.Error()})
}
