package telegram

import (
	"context"
	"fmt"
	"log/slog"

	tgbotapi "github.com/go-telegram-bot-api/telegram-bot-api/v5"
)

// handleCommand dispatches Telegram slash commands.
func (t *TelegramNotifier) handleCommand(ctx context.Context, msg *tgbotapi.Message) {
	switch msg.Command() {
	case "balance":
		t.sendBalance(ctx, msg.Chat.ID)
	case "gas":
		t.sendGas(ctx, msg.Chat.ID)
	case "positions":
		t.sendPositions(ctx, msg.Chat.ID)
	case "help":
		t.sendHelp(msg.Chat.ID)
	default:
		t.sendHelp(msg.Chat.ID)
	}
}

// sendBalance sends the current account balance message.
func (t *TelegramNotifier) sendBalance(ctx context.Context, chatID int64) {
	snap, err := t.info.FetchLiveSnapshot(ctx)
	if err != nil {
		slog.Warn("tgbot: fetch live snapshot for balance failed", "err", err)
		t.send(tgbotapi.NewMessage(chatID, fmt.Sprintf("查询失败，请稍后重试: %v", err)))
		return
	}
	m := tgbotapi.NewMessage(chatID, formatBalance(snap))
	m.ParseMode = tgbotapi.ModeMarkdown
	m.ReplyMarkup = balanceKeyboard()
	t.send(m)
}

// sendGas sends the wallet gas balance message.
func (t *TelegramNotifier) sendGas(ctx context.Context, chatID int64) {
	aptBal, _, err := t.info.GasBalance(ctx)
	walletAddr := t.info.WalletAddress()
	m := tgbotapi.NewMessage(chatID, formatGas(walletAddr, aptBal, err))
	m.ParseMode = tgbotapi.ModeMarkdown
	m.ReplyMarkup = gasKeyboard()
	t.send(m)
}

// sendPositions sends the current positions message.
func (t *TelegramNotifier) sendPositions(ctx context.Context, chatID int64) {
	snap, err := t.info.FetchLiveSnapshot(ctx)
	if err != nil {
		slog.Warn("tgbot: fetch live snapshot for positions failed", "err", err)
		t.send(tgbotapi.NewMessage(chatID, fmt.Sprintf("查询失败，请稍后重试: %v", err)))
		return
	}
	m := tgbotapi.NewMessage(chatID, formatPositions(snap))
	m.ParseMode = tgbotapi.ModeMarkdown
	m.ReplyMarkup = positionsKeyboard(snap)
	t.send(m)
}

// sendHelp sends the help/command list message.
func (t *TelegramNotifier) sendHelp(chatID int64) {
	m := tgbotapi.NewMessage(chatID, formatHelp(t.cfg))
	m.ParseMode = tgbotapi.ModeMarkdown
	m.ReplyMarkup = helpKeyboard()
	t.send(m)
}
