// Package telegram implements the notify.Notifier interface using the Telegram
// Bot API. It provides monitoring commands and inventory alerts.
package telegram

import (
	"context"
	"fmt"
	"log/slog"
	"math"
	"strings"

	tgbotapi "github.com/go-telegram-bot-api/telegram-bot-api/v5"

	"decibel-mm-bot/botstate"
	"decibel-mm-bot/notify"
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

func (t *TelegramNotifier) edit(chatID int64, msgID int, text string, kb *tgbotapi.InlineKeyboardMarkup) {
	edit := tgbotapi.NewEditMessageText(chatID, msgID, text)
	edit.ParseMode = tgbotapi.ModeMarkdown
	if kb != nil {
		edit.ReplyMarkup = kb
	}
	if _, err := t.api.Send(edit); err != nil {
		slog.Warn("tgbot: edit failed", "err", err)
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
		if t.isAdmin(update.Message.From.ID) {
			t.handleCommand(ctx, update.Message)
		}
	case update.CallbackQuery != nil:
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
	case "balance":
		snap := t.info.GetSnapshot()
		t.edit(chatID, msgID, formatBalance(snap), balanceKeyboard())

	case "gas":
		aptBal, _, err := t.info.GasBalance(ctx)
		walletAddr := t.info.WalletAddress()
		t.edit(chatID, msgID, formatGas(walletAddr, aptBal, err), gasKeyboard())

	case "positions":
		snap := t.info.GetSnapshot()
		t.edit(chatID, msgID, formatPositions(snap), positionsKeyboard(snap))

	case "close":
		t.handleCloseCallback(ctx, chatID, msgID)

	case "inv":
		t.handleInventoryCallback(ctx, cb, param)
	}
}

// handleCloseCallback places a flatten order and updates the message.
func (t *TelegramNotifier) handleCloseCallback(ctx context.Context, chatID int64, msgID int) {
	t.edit(chatID, msgID, "⏳ 正在平仓...", nil)

	snap := t.info.GetSnapshot()
	if err := t.info.FlattenPosition(ctx); err != nil {
		t.edit(chatID, msgID, fmt.Sprintf("❌ 平仓失败: %v", err), nil)
		return
	}

	midStr := "市价"
	if snap.Mid != nil {
		midStr = fmt.Sprintf("$%.2f", *snap.Mid)
	}
	t.edit(chatID, msgID,
		fmt.Sprintf("✅ 平仓单已提交 (~%s)\n仓位正在关闭中，请稍后刷新查看。", midStr),
		nil,
	)
}

// ── Keyboard builders ────────────────────────────────────────────────────────

func balanceKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "balance:refresh"),
		),
	)
	return &kb
}

func gasKeyboard() *tgbotapi.InlineKeyboardMarkup {
	kb := tgbotapi.NewInlineKeyboardMarkup(
		tgbotapi.NewInlineKeyboardRow(
			tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "gas:refresh"),
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
	))
	kb := tgbotapi.NewInlineKeyboardMarkup(rows...)
	return &kb
}
