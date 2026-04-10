package telegram

import (
	"context"
	"fmt"
	"log/slog"
	"math"
	"sync"
	"time"

	tgbotapi "github.com/go-telegram-bot-api/telegram-bot-api/v5"
)

// alertState tracks the single active inventory-limit alert message so that
// the loop edits it in-place rather than spamming new messages.
type alertState struct {
	mu          sync.Mutex
	activeMsgID int // Telegram message ID of the current alert; 0 = none sent yet
	chatID      int64
}

// runInventoryAlertLoop periodically checks whether the inventory exceeds the
// configured limit and sends (or updates) a Telegram alert message.
func (t *TelegramNotifier) runInventoryAlertLoop(ctx context.Context) {
	defer func() {
		if r := recover(); r != nil {
			slog.Error("tgbot: inventory alert loop panic", "err", r)
		}
	}()

	interval := time.Duration(t.cfg.AlertInventoryInterval) * time.Minute
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	as := &alertState{chatID: t.cfg.AdminID}

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			t.checkInventoryAlert(as)
		}
	}
}

// checkInventoryAlert reads the current state and sends or refreshes the alert.
// Network I/O (Telegram sends) is performed OUTSIDE the mutex to avoid blocking
// the lock while waiting for a remote response.
func (t *TelegramNotifier) checkInventoryAlert(as *alertState) {
	snap := t.info.GetSnapshot()
	maxInv := t.info.MaxInventory()
	exceeded := math.Abs(snap.Inventory) >= maxInv

	// Read activeMsgID under lock, then release before any network calls.
	as.mu.Lock()
	activeMsgID := as.activeMsgID
	as.mu.Unlock()

	if !exceeded {
		if activeMsgID != 0 {
			edit := tgbotapi.NewEditMessageText(as.chatID, activeMsgID,
				"✅ 仓位已恢复正常范围。")
			if _, err := t.api.Send(edit); err != nil {
				slog.Warn("tgbot: failed to update inventory alert (recovery)", "err", err)
			}
			as.mu.Lock()
			as.activeMsgID = 0
			as.mu.Unlock()
		}
		return
	}

	text := formatInventoryAlert(snap, maxInv)
	showClose := math.Abs(snap.Inventory) >= 1e-9
	kb := inventoryAlertKeyboard(showClose)

	if activeMsgID == 0 {
		// Send a brand-new alert message.
		m := tgbotapi.NewMessage(as.chatID, text)
		m.ParseMode = tgbotapi.ModeMarkdown
		m.ReplyMarkup = kb
		if sent, err := t.api.Send(m); err == nil {
			as.mu.Lock()
			as.activeMsgID = sent.MessageID
			as.mu.Unlock()
		} else {
			slog.Warn("tgbot: failed to send inventory alert", "err", err)
		}
	} else {
		// Edit the existing alert in-place (acts as "auto refresh").
		edit := tgbotapi.NewEditMessageText(as.chatID, activeMsgID, text)
		edit.ParseMode = tgbotapi.ModeMarkdown
		edit.ReplyMarkup = kb
		if _, err := t.api.Send(edit); err != nil {
			slog.Warn("tgbot: failed to refresh inventory alert", "err", err)
		}
	}
}

// handleInventoryCallback handles "inv:close" and "inv:refresh" button presses.
func (t *TelegramNotifier) handleInventoryCallback(ctx context.Context, cb *tgbotapi.CallbackQuery, action string) {
	chatID := cb.Message.Chat.ID
	msgID := cb.Message.MessageID

	snap := t.info.GetSnapshot()

	switch action {
	case "close":
		t.edit(chatID, msgID, "⏳ 正在平仓...", nil)
		if err := t.info.FlattenPosition(ctx); err != nil {
			t.edit(chatID, msgID, fmt.Sprintf("❌ 平仓失败: %v", err), nil)
			return
		}
		midStr := "市价"
		if snap.Mid != nil {
			midStr = fmt.Sprintf("$%.2f", *snap.Mid)
		}
		kb := inventoryAlertKeyboard(false)
		t.edit(chatID, msgID,
			fmt.Sprintf("✅ 平仓单已提交 (~%s)\n仓位正在关闭中。", midStr),
			kb,
		)

	case "refresh":
		maxInv := t.info.MaxInventory()
		exceeded := math.Abs(snap.Inventory) >= maxInv
		if !exceeded {
			t.edit(chatID, msgID, "✅ 仓位已恢复正常范围。", nil)
			return
		}
		text := formatInventoryAlert(snap, maxInv)
		showClose := math.Abs(snap.Inventory) >= 1e-9
		kb := inventoryAlertKeyboard(showClose)
		t.edit(chatID, msgID, text, kb)
	}
}

// inventoryAlertKeyboard builds the inline keyboard for the inventory alert.
func inventoryAlertKeyboard(showClose bool) *tgbotapi.InlineKeyboardMarkup {
	var row []tgbotapi.InlineKeyboardButton
	if showClose {
		row = append(row, tgbotapi.NewInlineKeyboardButtonData("❌ 市价平仓", "inv:close"))
	}
	row = append(row, tgbotapi.NewInlineKeyboardButtonData("🔄 刷新", "inv:refresh"))
	kb := tgbotapi.NewInlineKeyboardMarkup(tgbotapi.NewInlineKeyboardRow(row...))
	return &kb
}
