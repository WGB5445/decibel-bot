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
	"decibel-mm-bot/notify"
	"decibel-mm-bot/strategy"
)

// Config holds Telegram-specific configuration.
type Config struct {
	BotToken               string
	AdminID                int64
	AlertInventory         bool
	AlertInventoryInterval int // minutes
}

// TelegramNotifier implements notify.Notifier for Telegram.
type TelegramNotifier struct {
	cfg  Config
	api  *tgbotapi.BotAPI
	info notify.InfoProvider
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
			t.edit(chatID, msgID, formatHelp(t.cfg), helpKeyboard())
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
			t.editPlain(chatID, msgID, fmt.Sprintf("刷新失败: %v", err), balanceKeyboard())
			return
		}
		t.edit(chatID, msgID, formatBalance(snap), balanceKeyboard())

	case "gas":
		aptBal, _, err := t.info.GasBalance(ctx)
		walletAddr := t.info.WalletAddress()
		t.edit(chatID, msgID, formatGas(walletAddr, aptBal, err), gasKeyboard())

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
				t.editPlain(chatID, msgID, fmt.Sprintf("刷新失败: %v", err), positionsRefreshOnlyKeyboard())
				return
			}
			page = ClampPositionsPage(page, snap)
			t.edit(chatID, msgID, formatPositions(snap, page, t.info.MarketDisplayName), t.positionsReplyMarkup(snap, page))
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
			t.editPlain(chatID, msgID, fmt.Sprintf("查询失败: %v", err), tradesReplyMarkup(0, 0))
			return
		}
		page = ClampTradesPage(page, len(items))
		t.edit(chatID, msgID, formatRecentTrades(items, page, t.info.MarketDisplayName), tradesReplyMarkup(page, len(items)))

	case "close":
		t.handleCloseCallback(ctx, chatID, msgID)

	case "inv":
		t.handleInventoryCallback(ctx, cb, param)
	}
}

// pollTradeHistoryByOrder retries trade_history until rows appear or attempts exhaust.
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
		if len(items) > 0 {
			return items, nil
		}
	}
	return nil, lastErr
}

// flattenFollowUpMessage builds the post-flatten caption. If plain is non-empty, send it
// with editPlain (avoids Markdown breakage from arbitrary errors); otherwise use md with Markdown.
func (t *TelegramNotifier) flattenFollowUpMessage(ctx context.Context, txHash, orderID string, snap botstate.Snapshot) (md, plain string) {
	if t.info.DryRun() {
		return "*ℹ️ 模拟运行*\n未提交链上交易，无法查询成交历史。", ""
	}
	if strings.TrimSpace(orderID) == "" {
		return formatFlattenSubmittedNoHistory(snap, txHash, "", "未能从链上事件解析 order_id"), ""
	}
	items, err := t.pollTradeHistoryByOrder(ctx, snap.TargetMarketID, orderID)
	if err != nil {
		return "", fmt.Sprintf(
			"✅ 平仓单已提交\ntx: %s\norder_id: %s\n查询成交失败: %v",
			txHash, orderID, err,
		)
	}
	if len(items) == 0 {
		return formatFlattenSubmittedNoHistory(snap, txHash, orderID, "成交历史暂未索引到该订单"), ""
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
	return formatTradeFromHistory(*pick, snap.TargetMarketName, txHash), ""
}

// handleCloseCallback places a flatten order and updates the message.
func (t *TelegramNotifier) handleCloseCallback(ctx context.Context, chatID int64, msgID int) {
	t.edit(chatID, msgID, "正在平仓", positionsRefreshOnlyKeyboard())

	out, err := t.info.FlattenPosition(ctx)
	if err != nil {
		kb := positionsRefreshOnlyKeyboard()
		if snap, fetchErr := t.info.FetchLiveSnapshot(ctx); fetchErr == nil {
			kb = t.positionsReplyMarkup(snap, 0)
		}
		if errors.Is(err, strategy.ErrNoPositionToFlatten) {
			t.editPlain(chatID, msgID, "ℹ️ 当前目标市场无仓位或仓位过小，无需重复平仓。", kb)
			return
		}
		t.editPlain(chatID, msgID, fmt.Sprintf("❌ 平仓失败: %v", err), kb)
		return
	}

	snapLive, err2 := t.info.FetchLiveSnapshot(ctx)
	if err2 != nil {
		slog.Warn("tgbot: fetch live snapshot after flatten failed", "err", err2)
		t.editPlain(chatID, msgID, fmt.Sprintf("平仓已提交但刷新仓位失败: %v", err2), positionsRefreshOnlyKeyboard())
		return
	}
	md, plain := t.flattenFollowUpMessage(ctx, out.TxHash, out.OrderID, snapLive)
	// 平仓结果页：键盘触发「新开一条」仓位消息，本条成交提示不再被 edit。
	kb := positionsPostCloseKeyboard()
	if plain != "" {
		t.editPlain(chatID, msgID, plain, kb)
	} else {
		t.edit(chatID, msgID, md, kb)
	}
}

// ── Keyboard builders ────────────────────────────────────────────────────────

func balanceKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "balance:refresh"),
			tgbotapi.NewInlineKeyboardButtonData("🔙 返回", "menu:help"),
		),
	)
	return &kb
}

func gasKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "gas:refresh"),
			tgbotapi.NewInlineKeyboardButtonData("🔙 返回", "menu:help"),
		),
	)
	return &kb
}

func helpKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("💰 余额", "balance:help"),
			tgbotapi.NewInlineKeyboardButtonData("⛽ Gas", "gas:help"),
		),
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("📊 仓位", "positions:help"),
			tgbotapi.NewInlineKeyboardButtonData("📜 成交", "trades:help"),
		),
	)
	return &kb
}

// tradesReplyMarkup builds inline keys for paged recent trades (refresh resets to page 0).
func tradesReplyMarkup(page, itemCount int) *tgbotapi.InlineKeyboardMarkup {
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
		tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "trades:refresh"),
		tgbotapi.NewInlineKeyboardButtonData("🔙 返回", "menu:help"),
	))
	kb := tgbotapi.NewInlineKeyboardMarkup(rows...)
	return &kb
}

func positionsRefreshOnlyKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "positions:refresh"),
			tgbotapi.NewInlineKeyboardButtonData("🔙 返回", "menu:help"),
		),
	)
	return &kb
}

// positionsPostCloseKeyboard is used after a successful manual flatten: the message
// text is treated as final; "刷新" sends a new positions message instead of editing.
func positionsPostCloseKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "positions:newmsg"),
			tgbotapi.NewInlineKeyboardButtonData("🔙 返回", "menu:helpsend"),
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
		label := fmt.Sprintf("❌ 平仓 %s", snap.TargetMarketName)
		rows = append(rows, tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData(label, "close:"),
		))
		break
	}

	rows = append(rows, tgbotapi.NewInlineKeyboardRow(
		tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "positions:refresh"),
		tgbotapi.NewInlineKeyboardButtonData("🔙 返回", "menu:help"),
	))
	kb := tgbotapi.NewInlineKeyboardMarkup(rows...)
	return &kb
}
