package telegram

import (
	"context"

	tgbotapi "github.com/go-telegram-bot-api/telegram-bot-api/v5"
)

// handleCommand dispatches Telegram slash commands.
func (t *TelegramNotifier) handleCommand(ctx context.Context, msg *tgbotapi.Message) {
	switch msg.Command() {
	case "balance":
		t.sendBalance(msg.Chat.ID)
	case "gas":
		t.sendGas(ctx, msg.Chat.ID)
	case "positions":
		t.sendPositions(msg.Chat.ID)
	default:
		t.sendHelp(msg.Chat.ID)
	}
}

// sendBalance sends the current account balance message.
func (t *TelegramNotifier) sendBalance(chatID int64) {
	snap := t.info.GetSnapshot()
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
func (t *TelegramNotifier) sendPositions(chatID int64) {
	snap := t.info.GetSnapshot()
	m := tgbotapi.NewMessage(chatID, formatPositions(snap))
	m.ParseMode = tgbotapi.ModeMarkdown
	m.ReplyMarkup = positionsKeyboard(snap)
	t.send(m)
}

// sendHelp sends the help/command list message.
func (t *TelegramNotifier) sendHelp(chatID int64) {
	m := tgbotapi.NewMessage(chatID, formatHelp())
	m.ParseMode = tgbotapi.ModeMarkdown
	t.send(m)
}
