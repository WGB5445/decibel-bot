// Package telegram implements the notify.Notifier interface using the Telegram
// Bot API. It provides monitoring commands and inventory alerts.
package telegram

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"strings"

	tgbotapi "github.com/go-telegram-bot-api/telegram-bot-api/v5"

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
		snap, err := t.info.FetchLiveSnapshot(ctx)
		if err != nil {
			slog.Warn("tgbot: fetch live snapshot for positions callback failed", "err", err)
			t.editPlain(chatID, msgID, fmt.Sprintf("刷新失败: %v", err), positionsRefreshOnlyKeyboard())
			return
		}
		t.edit(chatID, msgID, formatPositions(snap), positionsKeyboard(snap))

	case "close":
		t.handleCloseCallback(ctx, chatID, msgID)

	case "inv":
		t.handleInventoryCallback(ctx, cb, param)
	}
}

// handleCloseCallback places a flatten order and updates the message.
func (t *TelegramNotifier) handleCloseCallback(ctx context.Context, chatID int64, msgID int) {
	t.edit(chatID, msgID, "⏳ 正在平仓...", positionsRefreshOnlyKeyboard())

	err := t.info.FlattenPosition(ctx)
	if err != nil {
		kb := positionsRefreshOnlyKeyboard()
		if snap, fetchErr := t.info.FetchLiveSnapshot(ctx); fetchErr == nil {
			kb = positionsKeyboard(snap)
		}
		if errors.Is(err, strategy.ErrNoPositionToFlatten) {
			t.editPlain(chatID, msgID, "ℹ️ 当前目标市场无仓位或仓位过小，无需重复平仓。", kb)
			return
		}
		t.editPlain(chatID, msgID, fmt.Sprintf("❌ 平仓失败: %v", err), kb)
		return
	}

	snapLive, err := t.info.FetchLiveSnapshot(ctx)
	if err != nil {
		slog.Warn("tgbot: fetch live snapshot after flatten failed", "err", err)
		t.edit(chatID, msgID,
			"✅ 平仓单已提交\n仓位正在关闭中，请点击刷新查看最新仓位。",
			positionsRefreshOnlyKeyboard(),
		)
		return
	}
	midStr := "市价"
	if snapLive.Mid != nil {
		midStr = fmt.Sprintf("$%.2f", *snapLive.Mid)
	}
	t.edit(chatID, msgID,
		fmt.Sprintf("✅ 平仓单已提交 (~%s)\n仓位正在关闭中，可点击下方刷新查看。", midStr),
		positionsKeyboard(snapLive),
	)
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
		),
	)
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

func positionsKeyboard(snap botstate.Snapshot) *tgbotapi.InlineKeyboardMarkup {
	var rows [][]tgbotapi.InlineKeyboardButton
	for _, p := range snap.AllPositions {
		if math.Abs(p.Size) < 1e-9 {
			continue
		}
		if !botstate.IDEqual(p.MarketID, snap.TargetMarketID) {
			continue
		}
		label := fmt.Sprintf("❌ 平仓 %s", snap.TargetMarketName)
		rows = append(rows, tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData(label, "close:"),
		))
	}
	rows = append(rows, tgbotapi.NewInlineKeyboardRow(
		tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "positions:refresh"),
		tgbotapi.NewInlineKeyboardButtonData("🔙 返回", "menu:help"),
	))
	kb := tgbotapi.NewInlineKeyboardMarkup(rows...)
	return &kb
}
