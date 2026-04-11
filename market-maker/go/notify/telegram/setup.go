package telegram

import (
	"context"
	"fmt"
	"log/slog"
	"time"

	tgbotapi "github.com/go-telegram-bot-api/telegram-bot-api/v5"
)

// telegramAPITimeout bounds blocking Bot API calls used during startup.
const telegramAPITimeout = 20 * time.Second

// Ready verifies connectivity to Telegram via getMe within the parent ctx and
// a bounded timeout. New() already calls GetMe once; this is an explicit
// pre-flight before the market-maker loop starts.
func (t *TelegramNotifier) Ready(ctx context.Context) error {
	rctx, cancel := context.WithTimeout(ctx, telegramAPITimeout)
	defer cancel()

	type res struct {
		user tgbotapi.User
		err  error
	}
	ch := make(chan res, 1)
	go func() {
		u, err := t.api.GetMe()
		ch <- res{user: u, err: err}
	}()

	select {
	case <-rctx.Done():
		return fmt.Errorf("telegram getMe: %w", rctx.Err())
	case out := <-ch:
		if out.err != nil {
			return fmt.Errorf("telegram getMe: %w", out.err)
		}
		name := out.user.UserName
		if name == "" {
			name = out.user.FirstName
		}
		slog.Info("telegram api ready", "bot", name)
		return nil
	}
}

// SetBotCommands registers slash commands with Telegram (setMyCommands) so they
// appear in the client when the user types "/". Scoped to all private chats.
func (t *TelegramNotifier) SetBotCommands(ctx context.Context) error {
	rctx, cancel := context.WithTimeout(ctx, telegramAPITimeout)
	defer cancel()

	scope := tgbotapi.NewBotCommandScopeAllPrivateChats()
	cmd := tgbotapi.NewSetMyCommandsWithScope(scope,
		tgbotapi.BotCommand{Command: "balance", Description: "查看账户余额"},
		tgbotapi.BotCommand{Command: "gas", Description: "查看钱包 APT 余额"},
		tgbotapi.BotCommand{Command: "positions", Description: "查看当前仓位"},
		tgbotapi.BotCommand{Command: "help", Description: "显示帮助"},
	)

	ch := make(chan error, 1)
	go func() {
		_, err := t.api.Request(cmd)
		ch <- err
	}()

	select {
	case <-rctx.Done():
		return fmt.Errorf("telegram setMyCommands: %w", rctx.Err())
	case err := <-ch:
		if err != nil {
			return fmt.Errorf("telegram setMyCommands: %w", err)
		}
		slog.Info("telegram bot commands registered")
		return nil
	}
}
