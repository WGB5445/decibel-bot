// Package telegram implements the notify.Notifier interface using the Telegram
// Bot API. It provides monitoring commands and inventory alerts.
package telegram

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"strconv"
	"strings"
	"time"

	tgbotapi "github.com/go-telegram-bot-api/telegram-bot-api/v5"

	"decibel-mm-bot/api"
	"decibel-mm-bot/botstate"
	"decibel-mm-bot/i18n"
	"decibel-mm-bot/notify"
	"decibel-mm-bot/strategy"
)

// Config holds Telegram-specific configuration.
type Config struct {
	BotToken               string
	AdminID                int64
	AlertInventory         bool
	AlertInventoryInterval int // minutes
	// Locale is "zh" (default) or "en"; from main/config (-locale / LOCALE / BOT_LOCALE).
	Locale string
}

// TelegramNotifier implements notify.Notifier for Telegram.
type TelegramNotifier struct {
	cfg  Config
	api  *tgbotapi.BotAPI
	info notify.InfoProvider
	tr   *i18n.Telegram
}

// New creates a TelegramNotifier and validates the bot token.
func New(cfg Config, info notify.InfoProvider) (*TelegramNotifier, error) {
	botAPI, err := tgbotapi.NewBotAPI(cfg.BotToken)
	if err != nil {
		return nil, fmt.Errorf("telegram bot API: %w", err)
	}
	return &TelegramNotifier{
		cfg:  cfg,
		api:  botAPI,
		info: info,
		tr:   i18n.Bundle(i18n.ParseLocale(cfg.Locale)),
	}, nil
}

// Run starts the long-poll update loop and, if configured, the inventory alert
// loop. It blocks until ctx is cancelled.
func (t *TelegramNotifier) Run(ctx context.Context) error {
	defer func() {
		if r := recover(); r != nil {
			slog.Error("tgbot: recovered from panic", "err", r)
		}
	}()

	if t.cfg.AlertInventory {
		go t.runInventoryAlertLoop(ctx)
	}

	u := tgbotapi.NewUpdate(0)
	u.Timeout = 30
	updates := t.api.GetUpdatesChan(u)

	for {
		select {
		case <-ctx.Done():
			t.api.StopReceivingUpdates()
			return nil
		case update, ok := <-updates:
			if !ok {
				return nil
			}
			t.handleUpdate(ctx, update)
		}
	}
}

// ── Security ─────────────────────────────────────────────────────────────────

func (t *TelegramNotifier) isAdmin(id int64) bool { return id == t.cfg.AdminID }

// ── Send helpers ─────────────────────────────────────────────────────────────

func (t *TelegramNotifier) send(msg tgbotapi.Chattable) {
	if _, err := t.api.Send(msg); err != nil {
		slog.Warn("tgbot: send failed", "err", err)
	}
}

// isTelegramMessageNotModified reports the Bot API case where editMessageText
// is a no-op because text and markup are unchanged — not a real failure.
func isTelegramMessageNotModified(err error) bool {
	if err == nil {
		return false
	}
	return strings.Contains(strings.ToLower(err.Error()), "message is not modified")
}

func (t *TelegramNotifier) edit(chatID int64, msgID int, text string, kb *tgbotapi.InlineKeyboardMarkup) {
	edit := tgbotapi.NewEditMessageText(chatID, msgID, text)
	edit.ParseMode = tgbotapi.ModeMarkdown
	if kb != nil {
		edit.ReplyMarkup = kb
	}
	if _, err := t.api.Send(edit); err != nil && !isTelegramMessageNotModified(err) {
		slog.Warn("tgbot: edit failed", "err", err)
	}
}

// editPlain updates message text without Markdown (safe for arbitrary error strings).
func (t *TelegramNotifier) editPlain(chatID int64, msgID int, text string, kb *tgbotapi.InlineKeyboardMarkup) {
	edit := tgbotapi.NewEditMessageText(chatID, msgID, text)
	if kb != nil {
		edit.ReplyMarkup = kb
	}
	if _, err := t.api.Send(edit); err != nil && !isTelegramMessageNotModified(err) {
		slog.Warn("tgbot: edit plain failed", "err", err)
	}
}

// ── Update dispatcher ────────────────────────────────────────────────────────

func (t *TelegramNotifier) handleUpdate(ctx context.Context, update tgbotapi.Update) {
	defer func() {
		if r := recover(); r != nil {
			slog.Error("tgbot: update handler panic", "err", r)
		}
	}()

	switch {
	case update.Message != nil && update.Message.IsCommand():
		// Only respond in private chats from admin
		if update.Message.Chat.Type != "private" {
			return
		}
		if t.isAdmin(update.Message.From.ID) {
			t.handleCommand(ctx, update.Message)
		}
	case update.CallbackQuery != nil:
		// Only respond to callbacks in private chats from admin
		if update.CallbackQuery.Message != nil && update.CallbackQuery.Message.Chat.Type != "private" {
			return
		}
		if t.isAdmin(update.CallbackQuery.From.ID) {
			t.handleCallback(ctx, update.CallbackQuery)
		}
	}
}

// ── Callback dispatcher ──────────────────────────────────────────────────────

func (t *TelegramNotifier) handleCallback(ctx context.Context, cb *tgbotapi.CallbackQuery) {
	// Always acknowledge the callback so Telegram removes the spinner.
	t.api.Request(tgbotapi.NewCallback(cb.ID, "")) //nolint:errcheck

	parts := strings.SplitN(cb.Data, ":", 2)
	action := parts[0]
	param := ""
	if len(parts) > 1 {
		param = parts[1]
	}

	chatID := cb.Message.Chat.ID
	msgID := cb.Message.MessageID

	switch action {
	case "menu":
		switch param {
		case "help":
			t.edit(chatID, msgID, formatHelp(t.cfg, t.tr), t.helpKeyboard())
		case "helpsend":
			// 新开一条帮助，避免 edit 覆盖已定格的平仓结果消息。
			t.sendHelp(chatID)
		default:
			slog.Warn("tgbot: unknown menu callback", "param", param)
		}

	case "balance":
		snap, err := t.info.FetchLiveSnapshot(ctx)
		if err != nil {
			slog.Warn("tgbot: fetch live snapshot for balance callback failed", "err", err)
			t.editPlain(chatID, msgID, fmt.Sprintf(t.tr.ErrRefreshFmt, err), t.balanceKeyboard())
			return
		}
		t.edit(chatID, msgID, formatBalance(t.tr, snap), t.balanceKeyboard())

	case "gas":
		aptBal, _, err := t.info.GasBalance(ctx)
		walletAddr := t.info.WalletAddress()
		t.edit(chatID, msgID, formatGas(t.tr, walletAddr, aptBal, err), t.gasKeyboard())

	case "positions":
		switch param {
		case "newmsg":
			// 平仓结果页专用：发送新消息展示仓位，不 edit 当前消息。
			t.sendPositions(ctx, chatID)
		default:
			page := 0
			switch {
			case param == "refresh" || param == "help" || param == "":
				page = 0
			case strings.HasPrefix(param, "pg:"):
				n, err := strconv.Atoi(strings.TrimPrefix(param, "pg:"))
				if err != nil {
					slog.Warn("tgbot: bad positions page", "param", param, "err", err)
					page = 0
				} else {
					page = n
				}
			default:
				slog.Warn("tgbot: unknown positions callback", "param", param)
				return
			}
			snap, err := t.info.FetchLiveSnapshot(ctx)
			if err != nil {
				slog.Warn("tgbot: fetch live snapshot for positions callback failed", "err", err)
				t.editPlain(chatID, msgID, fmt.Sprintf(t.tr.ErrRefreshFmt, err), t.positionsRefreshOnlyKeyboard())
				return
			}
			page = ClampPositionsPage(page, snap)
			t.edit(chatID, msgID, formatPositions(t.tr, snap, page, t.info.MarketDisplayName), t.positionsReplyMarkup(snap, page))
		}

	case "trades":
		page := 0
		switch {
		case param == "refresh" || param == "help" || param == "":
			page = 0
		case strings.HasPrefix(param, "pg:"):
			n, err := strconv.Atoi(strings.TrimPrefix(param, "pg:"))
			if err != nil {
				slog.Warn("tgbot: bad trades page", "param", param, "err", err)
				page = 0
			} else {
				page = n
			}
		default:
			slog.Warn("tgbot: unknown trades callback", "param", param)
			return
		}
		items, err := t.info.FetchRecentTrades(ctx, TradesHistoryFetchLimit)
		if err != nil {
			slog.Warn("tgbot: fetch recent trades failed", "err", err)
			t.editPlain(chatID, msgID, fmt.Sprintf(t.tr.ErrQueryFmt, err), t.tradesReplyMarkup(0, 0))
			return
		}
		page = ClampTradesPage(page, len(items))
		t.edit(chatID, msgID, formatRecentTrades(t.tr, items, page, t.info.MarketDisplayName), t.tradesReplyMarkup(page, len(items)))

	case "close":
		t.handleCloseCallback(ctx, chatID, msgID)

	case "inv":
		t.handleInventoryCallback(ctx, cb, param)
	}
}

// pollTradeHistoryByOrder retries trade_history until rows appear or attempts exhaust.
// On success with rows, returns the items and a nil error.
// After all attempts: if the last fetch attempt failed, returns (nil, lastErr).
// If every attempt succeeded but returned no rows, returns an empty non-nil slice and nil error
// (never (nil, nil)), so callers can rely on len(items)==0 without a three-valued ambiguity.
func (t *TelegramNotifier) pollTradeHistoryByOrder(ctx context.Context, marketAddr, orderID string) ([]api.TradeHistoryItem, error) {
	var lastErr error
	for attempt := 0; attempt < 12; attempt++ {
		if attempt > 0 {
			select {
			case <-ctx.Done():
				return nil, ctx.Err()
			case <-time.After(400 * time.Millisecond):
			}
		}
		items, err := t.info.FetchTradeHistoryByOrder(ctx, marketAddr, orderID)
		if err != nil {
			lastErr = err
			slog.Warn("tgbot: trade_history poll failed", "err", err, "attempt", attempt+1)
			continue
		}
		lastErr = nil // successful HTTP decode: do not carry stale errors from earlier attempts
		if len(items) > 0 {
			return items, nil
		}
	}
	if lastErr != nil {
		return nil, lastErr
	}
	return []api.TradeHistoryItem{}, nil
}

// flattenFollowUpMessage builds the post-flatten caption. If plain is non-empty, send it
// with editPlain (avoids Markdown breakage from arbitrary errors); otherwise use md with Markdown.
func (t *TelegramNotifier) flattenFollowUpMessage(ctx context.Context, txHash, orderID string, snap botstate.Snapshot) (md, plain string) {
	if t.info.DryRun() {
		return t.tr.DryRunNoHistory, ""
	}
	if strings.TrimSpace(orderID) == "" {
		return formatFlattenSubmittedNoHistory(t.tr, snap, txHash, "", t.tr.ReasonOrderIDParse), ""
	}
	items, err := t.pollTradeHistoryByOrder(ctx, snap.TargetMarketID, orderID)
	if err != nil {
		return "", fmt.Sprintf(
			t.tr.FlattenQueryFailFmt,
			txHash, orderID, err,
		)
	}
	if len(items) == 0 {
		return formatFlattenSubmittedNoHistory(t.tr, snap, txHash, orderID, t.tr.ReasonTradePending), ""
	}
	var pick *api.TradeHistoryItem
	for i := range items {
		if items[i].OrderID == orderID {
			pick = &items[i]
			break
		}
	}
	if pick == nil {
		pick = &items[0]
	}
	return formatTradeFromHistory(t.tr, *pick, snap.TargetMarketName, txHash), ""
}

// handleCloseCallback places a flatten order and updates the message.
func (t *TelegramNotifier) handleCloseCallback(ctx context.Context, chatID int64, msgID int) {
	t.edit(chatID, msgID, t.tr.Flattening, t.positionsRefreshOnlyKeyboard())

	out, err := t.info.FlattenPosition(ctx)
	if err != nil {
		kb := t.positionsRefreshOnlyKeyboard()
		if snap, fetchErr := t.info.FetchLiveSnapshot(ctx); fetchErr == nil {
			kb = t.positionsReplyMarkup(snap, 0)
		}
		if errors.Is(err, strategy.ErrNoPositionToFlatten) {
			t.editPlain(chatID, msgID, t.tr.FlattenNoNeed, kb)
			return
		}
		t.editPlain(chatID, msgID, fmt.Sprintf(t.tr.FlattenFailFmt, err), kb)
		return
	}

	snapLive, err2 := t.info.FetchLiveSnapshot(ctx)
	if err2 != nil {
		slog.Warn("tgbot: fetch live snapshot after flatten failed", "err", err2)
		t.editPlain(chatID, msgID, fmt.Sprintf(t.tr.FlattenPosRefreshFailFmt, err2), t.positionsRefreshOnlyKeyboard())
		return
	}
	md, plain := t.flattenFollowUpMessage(ctx, out.TxHash, out.OrderID, snapLive)
	kb := t.positionsPostCloseKeyboard()
	if plain != "" {
		t.editPlain(chatID, msgID, plain, kb)
	} else {
		t.edit(chatID, msgID, md, kb)
	}
}

// ── Keyboard builders ────────────────────────────────────────────────────────

func (t *TelegramNotifier) balanceKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnRefresh, "balance:refresh"),
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnBack, "menu:help"),
		),
	)
	return &kb
}

func (t *TelegramNotifier) gasKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnRefresh, "gas:refresh"),
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnBack, "menu:help"),
		),
	)
	return &kb
}

func (t *TelegramNotifier) helpKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnBalance, "balance:help"),
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnGas, "gas:help"),
		),
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnPositions, "positions:help"),
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnTrades, "trades:help"),
		),
	)
	return &kb
}

// tradesReplyMarkup builds inline keys for paged recent trades (refresh resets to page 0).
func (t *TelegramNotifier) tradesReplyMarkup(page, itemCount int) *tgbotapi.InlineKeyboardMarkup {
	page = ClampTradesPage(page, itemCount)
	tp := TradesTotalPages(itemCount)
	var rows [][]tgbotapi.InlineKeyboardButton
	if tp > 1 {
		prev := page - 1
		if prev < 0 {
			prev = 0
		}
		next := page + 1
		if next >= tp {
			next = tp - 1
		}
		rows = append(rows, tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("◀️", fmt.Sprintf("trades:pg:%d", prev)),
			tgbotapi.NewInlineKeyboardButtonData("▶️", fmt.Sprintf("trades:pg:%d", next)),
		))
	}
	rows = append(rows, tgbotapi.NewInlineKeyboardRow(
		tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnRefresh, "trades:refresh"),
		tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnBack, "menu:help"),
	))
	kb := tgbotapi.NewInlineKeyboardMarkup(rows...)
	return &kb
}

func (t *TelegramNotifier) positionsRefreshOnlyKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnRefresh, "positions:refresh"),
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnBack, "menu:help"),
		),
	)
	return &kb
}

// positionsPostCloseKeyboard is used after a successful manual flatten: the message
// text is treated as final; "刷新" sends a new positions message instead of editing.
func (t *TelegramNotifier) positionsPostCloseKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnRefresh, "positions:newmsg"),
			tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnBack, "menu:helpsend"),
		),
	)
	return &kb
}

// positionsReplyMarkup builds inline keys for the paged positions view.
func (t *TelegramNotifier) positionsReplyMarkup(snap botstate.Snapshot, page int) *tgbotapi.InlineKeyboardMarkup {
	page = ClampPositionsPage(page, snap)
	tp := PositionsTotalPages(snap)
	var rows [][]tgbotapi.InlineKeyboardButton

	if tp > 1 {
		prev := page - 1
		if prev < 0 {
			prev = 0
		}
		next := page + 1
		if next >= tp {
			next = tp - 1
		}
		rows = append(rows, tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("◀️", fmt.Sprintf("positions:pg:%d", prev)),
			tgbotapi.NewInlineKeyboardButtonData("▶️", fmt.Sprintf("positions:pg:%d", next)),
		))
	}

	for _, p := range positionsForDisplay(snap) {
		if !botstate.IDEqual(p.MarketID, snap.TargetMarketID) {
			continue
		}
		label := fmt.Sprintf(t.tr.BtnCloseMarketFmt, snap.TargetMarketName)
		rows = append(rows, tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData(label, "close:"),
		))
		break
	}

	rows = append(rows, tgbotapi.NewInlineKeyboardRow(
		tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnRefresh, "positions:refresh"),
		tgbotapi.NewInlineKeyboardButtonData(t.tr.BtnBack, "menu:help"),
	))
	kb := tgbotapi.NewInlineKeyboardMarkup(rows...)
	return &kb
}
