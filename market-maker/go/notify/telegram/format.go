// Package telegram — formatting functions. All functions in this file are pure
// (no I/O, no side effects).
package telegram

import (
	"fmt"
	"math"
	"strings"
	"time"

	"decibel-mm-bot/botstate"
)

// ── Balance ──────────────────────────────────────────────────────────────────

func formatBalance(snap botstate.Snapshot) string {
	available := snap.Equity * (1.0 - snap.MarginUsage)
	return fmt.Sprintf(
		"*💰 账户余额*\n"+
			"可用余额: `$%.2f`\n"+
			"总权益: `$%.2f`\n"+
			"保证金占用: `%.1f%%`\n"+
			"_%s_",
		available, snap.Equity, snap.MarginUsage*100, cycleAge(snap.LastCycleAt),
	)
}

// ── Gas ──────────────────────────────────────────────────────────────────────

func formatGas(walletAddr string, aptBal float64, err error) string {
	short := walletAddr
	if len(short) > 16 {
		short = short[:8] + "..." + short[len(short)-6:]
	}
	if err != nil {
		return fmt.Sprintf("*⛽ Gas 钱包*\n地址: `%s`\n❌ 查询失败: %v", short, err)
	}
	return fmt.Sprintf("*⛽ Gas 钱包*\n地址: `%s`\nAPT 余额: `%.4f APT`", short, aptBal)
}

// ── Positions ────────────────────────────────────────────────────────────────

func formatPositions(snap botstate.Snapshot) string {
	hasNonZero := false
	for _, p := range snap.AllPositions {
		if math.Abs(p.Size) >= 1e-9 {
			hasNonZero = true
			break
		}
	}
	if !hasNonZero {
		return "*📊 当前仓位*\n暂无持仓。"
	}

	var sb strings.Builder
	sb.WriteString("*📊 当前仓位*\n")
	for _, p := range snap.AllPositions {
		if math.Abs(p.Size) < 1e-9 {
			continue
		}
		dir := "LONG ▲"
		if p.Size < 0 {
			dir = "SHORT ▼"
		}
		name := p.MarketID
		if botstate.IDEqual(p.MarketID, snap.TargetMarketID) {
			name = snap.TargetMarketName
		}
		sb.WriteString(fmt.Sprintf("• *%s*  %s  `%.5f`\n", escapeMarkdown(name), dir, math.Abs(p.Size)))

		if botstate.IDEqual(p.MarketID, snap.TargetMarketID) {
			if snap.Mid != nil {
				sb.WriteString(fmt.Sprintf("  当前价: `$%.2f`\n", *snap.Mid))
			}
			if snap.EntryPrice > 0 && snap.Mid != nil {
				pnl := (*snap.Mid - snap.EntryPrice) * p.Size
				pct := (*snap.Mid - snap.EntryPrice) / snap.EntryPrice * 100
				sb.WriteString(fmt.Sprintf("  ~盈亏: %s\n", formatPnL(pnl, pct)))
			}
		}
	}
	sb.WriteString(fmt.Sprintf("_%s_", cycleAge(snap.LastCycleAt)))
	return sb.String()
}

// ── Inventory alert ──────────────────────────────────────────────────────────

func formatInventoryAlert(snap botstate.Snapshot, maxInventory float64) string {
	dir := "LONG ▲"
	if snap.Inventory < 0 {
		dir = "SHORT ▼"
	}
	midStr := "N/A"
	if snap.Mid != nil {
		midStr = fmt.Sprintf("$%.2f", *snap.Mid)
	}

	var pnlLine string
	if snap.EntryPrice > 0 && snap.Mid != nil {
		pnl := (*snap.Mid - snap.EntryPrice) * snap.Inventory
		pct := (*snap.Mid - snap.EntryPrice) / snap.EntryPrice * 100
		pnlLine = fmt.Sprintf("\n~盈亏: %s", formatPnL(pnl, pct))
	}

	return fmt.Sprintf(
		"⚠️ *仓位超限提醒*\n"+
			"市场: `%s`\n"+
			"方向: `%s`\n"+
			"仓位: `%.5f` (限制: `%.5f`)"+
			"\n当前价: `%s`%s",
		escapeMarkdown(snap.TargetMarketName), dir,
		math.Abs(snap.Inventory), maxInventory,
		midStr, pnlLine,
	)
}

// ── Help ─────────────────────────────────────────────────────────────────────

func formatHelp() string {
	return "*🤖 Decibel 做市机器人*\n\n" +
		"可用命令:\n" +
		"/balance — 查看账户余额\n" +
		"/gas — 查看钱包 APT 余额\n" +
		"/positions — 查看当前仓位\n" +
		"/help — 显示帮助"
}

// ── Internal helpers ─────────────────────────────────────────────────────────

func cycleAge(lastCycleAt time.Time) string {
	if lastCycleAt.IsZero() {
		return "正在获取..."
	}
	return fmt.Sprintf("更新于 %s 前", time.Since(lastCycleAt).Truncate(time.Second))
}

func formatPnL(pnl, pct float64) string {
	sign := "+"
	if pnl < 0 {
		sign = ""
	}
	return fmt.Sprintf("`%s$%.2f (%s%.2f%%)`", sign, pnl, sign, pct)
}

// escapeMarkdown escapes Telegram MarkdownV1 special characters.
// Special chars: _ * ` [
func escapeMarkdown(s string) string {
	r := strings.NewReplacer(
		"_", "\\_",
		"*", "\\*",
		"`", "\\`",
		"[", "\\[",
	)
	return r.Replace(s)
}
