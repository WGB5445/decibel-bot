package telegram

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"sync"
	"time"

	tgbotapi "github.com/go-telegram-bot-api/telegram-bot-api/v5"

	"decibel-mm-bot/strategy"
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
			if _, err := t.api.Send(edit); err != nil && !isTelegramMessageNotModified(err) {
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
		if _, err := t.api.Send(edit); err != nil && !isTelegramMessageNotModified(err) {
			slog.Warn("tgbot: failed to refresh inventory alert", "err", err)
		}
	}
}

// handleInventoryCallback handles "inv:close" and "inv:refresh" button presses.
func (t *TelegramNotifier) handleInventoryCallback(ctx context.Context, cb *tgbotapi.CallbackQuery, action string) {
	chatID := cb.Message.Chat.ID
	msgID := cb.Message.MessageID

	switch action {
	case "close":
		t.edit(chatID, msgID, "正在平仓", inventoryAlertKeyboard(false))
		out, err := t.info.FlattenPosition(ctx)
		if err != nil {
			kb := inventoryAlertKeyboard(false)
			if snap, fe := t.info.FetchLiveSnapshot(ctx); fe == nil {
				kb = inventoryAlertKeyboard(math.Abs(snap.Inventory) >= 1e-9)
			}
			if errors.Is(err, strategy.ErrNoPositionToFlatten) {
				t.editPlain(chatID, msgID, "ℹ️ 当前目标市场无仓位或仓位过小，无需重复平仓。", kb)
				return
			}
			t.editPlain(chatID, msgID, fmt.Sprintf("❌ 平仓失败: %v", err), kb)
			return
		}
		snap, err2 := t.info.FetchLiveSnapshot(ctx)
		if err2 != nil {
			slog.Warn("tgbot: fetch live snapshot after inv flatten failed", "err", err2)
			t.editPlain(chatID, msgID, fmt.Sprintf("平仓已提交但刷新失败: %v", err2), inventoryAlertKeyboard(true))
			return
		}
		// 平仓结果定格：刷新走新消息仓位；返回走新消息帮助，不 edit 本条。
		kb := positionsPostCloseKeyboard()
		md, plain := t.flattenFollowUpMessage(ctx, out.TxHash, out.OrderID, snap)
		if plain != "" {
			t.editPlain(chatID, msgID, plain, kb)
		} else {
			t.edit(chatID, msgID, md, kb)
		}

	case "refresh":
		snap, err := t.info.FetchLiveSnapshot(ctx)
		if err != nil {
			slog.Warn("tgbot: inv refresh fetch live snapshot failed", "err", err)
			t.editPlain(chatID, msgID, fmt.Sprintf("刷新失败: %v", err), inventoryAlertKeyboard(true))
			return
		}
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
