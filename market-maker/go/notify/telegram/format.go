// Package telegram — formatting functions. All functions in this file are pure
// (no I/O, no side effects).
package telegram

import (
	"fmt"
	"math"
	"sort"
	"strings"
	"time"
	"unicode"

	"decibel-mm-bot/api"
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
	if err != nil {
		return fmt.Sprintf("*⛽ Gas 钱包*\n地址: `%s`\n❌ 查询失败: %v", walletAddr, err)
	}
	return fmt.Sprintf("*⛽ Gas 钱包*\n地址: `%s`\nAPT 余额: `%.4f APT`", walletAddr, aptBal)
}

// ── Positions ────────────────────────────────────────────────────────────────

// PositionsPageSize is the number of non-zero positions shown per Telegram page.
const PositionsPageSize = 3

func positionsForDisplay(snap botstate.Snapshot) []botstate.Position {
	var out []botstate.Position
	for _, p := range snap.AllPositions {
		if p.IsDeleted {
			continue
		}
		if math.Abs(p.Size) < 1e-9 {
			continue
		}
		out = append(out, p)
	}
	sort.Slice(out, func(i, j int) bool {
		return strings.ToLower(out[i].MarketID) < strings.ToLower(out[j].MarketID)
	})
	return out
}

// PositionsTotalPages returns total pages for the given snapshot (at least 1).
func PositionsTotalPages(snap botstate.Snapshot) int {
	n := len(positionsForDisplay(snap))
	if n == 0 {
		return 1
	}
	return (n + PositionsPageSize - 1) / PositionsPageSize
}

// ClampPositionsPage clamps page index to [0, totalPages-1].
func ClampPositionsPage(page int, snap botstate.Snapshot) int {
	tp := PositionsTotalPages(snap)
	if tp <= 0 {
		return 0
	}
	if page < 0 {
		return 0
	}
	if page >= tp {
		return tp - 1
	}
	return page
}

// telegramPlainDisplayWidth approximates monospace/column width for padding
// the positions header (Telegram uses proportional fonts; this is best-effort).
func telegramPlainDisplayWidth(s string) int {
	w := 0
	for _, r := range s {
		switch {
		case r <= 0x007F:
			w++
		case unicode.Is(unicode.Han, r) || unicode.Is(unicode.Hangul, r):
			w += 2
		case r >= 0x3040 && r <= 0x30FF: // Hiragana / Katakana
			w += 2
		case r >= 0x1F300 && r <= 0x1FAFF: // common emoji blocks
			w += 2
		default:
			w++
		}
	}
	return w
}

// markdownHeadingWithPage renders one Markdown line: bold left, padded spaces, italic page.
func markdownHeadingWithPage(leftBold string, page1Based, totalPages int) string {
	right := fmt.Sprintf("_第 %d/%d 页_", page1Based, totalPages)
	leftInner := strings.TrimSuffix(strings.TrimPrefix(leftBold, "*"), "*")
	rightInner := strings.TrimSuffix(strings.TrimPrefix(right, "_"), "_")
	lw := telegramPlainDisplayWidth(leftInner)
	rw := telegramPlainDisplayWidth(rightInner)
	const targetCells = 36
	sp := targetCells - lw - rw
	if sp < 4 {
		sp = 4
	}
	return leftBold + strings.Repeat(" ", sp) + right
}

// positionsHeadingLine is one Markdown line: bold title left, italic page right.
func positionsHeadingLine(page1Based, totalPages int) string {
	return markdownHeadingWithPage("*📊 当前仓位*", page1Based, totalPages)
}

func positionMid(snap botstate.Snapshot, marketID string) (float64, bool) {
	if botstate.IDEqual(marketID, snap.TargetMarketID) && snap.Mid != nil {
		return *snap.Mid, true
	}
	if snap.MidByMarket == nil {
		return 0, false
	}
	mid, ok := snap.MidByMarket[api.NormalizeAddr(marketID)]
	return mid, ok
}

func positionNotional(p botstate.Position, markMid float64, ok bool) (float64, bool) {
	switch {
	case ok:
		return math.Abs(p.Size) * markMid, true
	case p.EntryPrice > 0:
		return math.Abs(p.Size) * p.EntryPrice, true
	default:
		return 0, false
	}
}

func positionPnL(p botstate.Position, markMid float64) (float64, float64, bool) {
	if p.EntryPrice <= 0 || math.Abs(p.Size) < 1e-9 {
		return 0, 0, false
	}
	pnl := (markMid - p.EntryPrice) * p.Size
	base := math.Abs(p.EntryPrice * p.Size)
	if base <= 1e-9 {
		return pnl, 0, false
	}
	return pnl, pnl / base * 100, true
}

func positionDirectionDisplay(size float64) string {
	if size < 0 {
		return "空 ▼"
	}
	return "多 ▲"
}

func positionPnLLabel(pnl float64) string {
	if pnl < 0 {
		return "亏损"
	}
	return "盈利"
}

// positionsEmptySpacer adds vertical whitespace so the “no positions” message is
// closer in height to a typical filled view (Telegram may collapse pure blank lines).
func positionsEmptySpacer() string {
	const lines = 14
	var b strings.Builder
	b.Grow(lines * 2)
	for range lines {
		b.WriteString("\u200b\n")
	}
	return b.String()
}

// formatPositions renders the positions view for Telegram (paged).
// marketName resolves market addr to display name (e.g. from /markets catalog).
// page is zero-based; refresh should pass 0 per product spec.
func formatPositions(snap botstate.Snapshot, page int, marketName func(string) string) string {
	if marketName == nil {
		marketName = func(addr string) string {
			if botstate.IDEqual(addr, snap.TargetMarketID) {
				return snap.TargetMarketName
			}
			return addr
		}
	}
	list := positionsForDisplay(snap)
	if len(list) == 0 {
		var sb strings.Builder
		sb.WriteString(positionsHeadingLine(1, 1))
		sb.WriteString("\n\n")
		sb.WriteString("_暂无持仓_\n")
		sb.WriteString(positionsEmptySpacer())
		sb.WriteString(fmt.Sprintf("\n_%s_", cycleAge(snap.LastCycleAt)))
		return sb.String()
	}
	page = ClampPositionsPage(page, snap)
	tp := PositionsTotalPages(snap)
	start := page * PositionsPageSize
	end := start + PositionsPageSize
	if end > len(list) {
		end = len(list)
	}

	var sb strings.Builder
	sb.WriteString(positionsHeadingLine(page+1, tp))
	sb.WriteString("\n\n")

	for i, p := range list[start:end] {
		if i > 0 {
			sb.WriteString("  ──────────────\n")
		}
		label := marketName(p.MarketID)
		markMid, hasMid := positionMid(snap, p.MarketID)
		notional, hasNotional := positionNotional(p, markMid, hasMid)
		dir := positionDirectionDisplay(p.Size)
		sb.WriteString(fmt.Sprintf("• *%s* · *%s*\n", escapeMarkdown(label), dir))
		sb.WriteString(fmt.Sprintf("  持仓 `%.5f` · 杠杆 `%.0fx`\n", math.Abs(p.Size), p.UserLeverage))

		switch {
		case p.EntryPrice > 0 && hasMid:
			sb.WriteString(fmt.Sprintf("  开仓价 `$%.2f` · 当前价 `$%.2f`\n", p.EntryPrice, markMid))
		case p.EntryPrice > 0:
			sb.WriteString(fmt.Sprintf("  开仓价 `$%.2f` · 当前价 `—`\n", p.EntryPrice))
		case hasMid:
			sb.WriteString(fmt.Sprintf("  当前价 `$%.2f`\n", markMid))
		}

		switch {
		case hasNotional && hasMid:
			sb.WriteString(fmt.Sprintf("  持仓价值 `$%.2f`\n", notional))
			pnl, pct, ok := positionPnL(p, markMid)
			if ok {
				sb.WriteString(fmt.Sprintf("  %s(估算) %s\n", positionPnLLabel(pnl), formatPnL(pnl, pct)))
			} else {
				sb.WriteString("  盈亏(估算) `—`\n")
			}
		case hasNotional:
			sb.WriteString(fmt.Sprintf("  持仓价值 `$%.2f`\n", notional))
			sb.WriteString("  盈亏(估算) `—`\n")
		case hasMid:
			if pnl, pct, ok := positionPnL(p, markMid); ok {
				sb.WriteString(fmt.Sprintf("  %s(估算) %s\n", positionPnLLabel(pnl), formatPnL(pnl, pct)))
			} else {
				sb.WriteString("  盈亏(估算) `—`\n")
			}
		}

		var extras []string
		if p.UnrealizedFunding != 0 {
			extras = append(extras, fmt.Sprintf("资金费 `$%.4f`", p.UnrealizedFunding))
		}
		if p.EstimatedLiquidationPrice > 0 {
			extras = append(extras, fmt.Sprintf("强平 `$%.2f`", p.EstimatedLiquidationPrice))
		}
		if len(extras) > 0 {
			sb.WriteString("  " + strings.Join(extras, " · ") + "\n")
		}
	}
	sb.WriteString(fmt.Sprintf("\n_%s_", cycleAge(snap.LastCycleAt)))
	return sb.String()
}

// telegramPreBlock wraps body in a Markdown pre block (```) so the user can
// long-press and copy one contiguous region. Body must not contain raw ```.
func telegramPreBlock(body string) string {
	body = strings.ReplaceAll(body, "```", "``\u200d``")
	return "```\n" + body + "\n```"
}

// formatTradeFromHistory formats one trade_history row after a successful flatten.
// Source is intentionally omitted in Telegram UI (no "来源" line).
func formatTradeFromHistory(tr api.TradeHistoryItem, marketName, txHash string) string {
	ts := "—"
	if tr.TransactionUnixMs > 0 {
		ts = time.UnixMilli(tr.TransactionUnixMs).In(time.Local).Format("2006-01-02 15:04:05")
	}
	action := tradeActionDisplay(tr.Action)
	var sb strings.Builder
	sb.WriteString("*✅ 平仓成交*\n\n")
	sb.WriteString(fmt.Sprintf("• *%s*\n", escapeMarkdown(marketName)))
	sb.WriteString(fmt.Sprintf("  *%s*\n", action))
	sb.WriteString(fmt.Sprintf("  成交价 `$%.4f` · 数量 `%.4f`\n", tr.Price, tr.Size))
	sb.WriteString(fmt.Sprintf("  实现盈亏 `$%.4f`\n", tr.RealizedPnlAmount))
	sb.WriteString(fmt.Sprintf("  资金费 `$%.4f` · 手续费 `$%.4f`\n", tr.RealizedFundingAmount, tr.FeeAmount))
	sb.WriteString(fmt.Sprintf("  时间 `%s`\n", escapeMarkdown(ts)))
	sb.WriteString("tx:\n")
	sb.WriteString(telegramPreBlock(strings.TrimSpace(txHash)))
	sb.WriteString("\n_本条为平仓结果，不再更新。点「刷新」在下方新消息查看仓位。_")
	return sb.String()
}

// formatFlattenSubmittedNoHistory is shown when the chain accepted the order but
// REST has no matching trade row yet (or order_id unknown).
func formatFlattenSubmittedNoHistory(snap botstate.Snapshot, txHash, orderID string, reason string) string {
	midStr := "市价"
	if snap.Mid != nil {
		midStr = fmt.Sprintf("$%.2f", *snap.Mid)
	}
	var extra string
	if strings.TrimSpace(orderID) != "" {
		extra = "\norder_id:\n" + telegramPreBlock(strings.TrimSpace(orderID))
	}
	if strings.TrimSpace(reason) != "" {
		extra += "\n_" + escapeMarkdown(reason) + "_"
	}
	return fmt.Sprintf(
		"*✅ 平仓单已提交* (~%s)\n"+
			"tx:\n%s%s\n"+
			"请稍后点「刷新」在新消息查看仓位，或点「成交」查看历史。\n"+
			"_本条为平仓结果，不再更新。_\n"+
			"_%s_",
		midStr, telegramPreBlock(strings.TrimSpace(txHash)), extra, cycleAge(snap.LastCycleAt),
	)
}

// ── Recent trades (Telegram) ───────────────────────────────────────────────

// TradesPageSize is how many fills to show per Telegram page.
const TradesPageSize = 5

// TradesHistoryFetchLimit is how many rows we pull from REST for in-memory paging.
const TradesHistoryFetchLimit = 100

func normTradeAction(action string) string {
	s := strings.TrimSpace(action)
	s = strings.ReplaceAll(s, " ", "")
	s = strings.ReplaceAll(s, "_", "")
	return strings.ToLower(s)
}

// tradeActionDisplay maps API action strings to short Chinese labels for Telegram.
func tradeActionDisplay(action string) string {
	switch normTradeAction(action) {
	case "closelong":
		return "平多"
	case "closeshort":
		return "平空"
	case "openlong":
		return "开多"
	case "openshort":
		return "开空"
	default:
		a := strings.TrimSpace(action)
		if a == "" {
			return "—"
		}
		return a
	}
}

// TradesTotalPages returns pages for itemCount fills (at least 1 when itemCount==0 for UI).
func TradesTotalPages(itemCount int) int {
	if itemCount <= 0 {
		return 1
	}
	return (itemCount + TradesPageSize - 1) / TradesPageSize
}

// ClampTradesPage clamps zero-based page to [0, totalPages-1] for itemCount rows.
func ClampTradesPage(page, itemCount int) int {
	tp := TradesTotalPages(itemCount)
	if page < 0 {
		return 0
	}
	if page >= tp {
		return tp - 1
	}
	return page
}

func tradesHeadingLine(page1Based, totalPages int) string {
	return markdownHeadingWithPage("*📜 最近成交*", page1Based, totalPages)
}

// tradeCardSeparator is a plain-text rule between fills (Telegram legacy Markdown).
const tradeCardSeparator = "──────────────"

// zwsp prefixes a line that starts with "1. " so Telegram legacy Markdown does not
// treat it as an auto-ordered list (avoids visible backslash escapes).
const telegramLineNoListPrefix = "\u200b"

func tradeMarketForCode(mname string) string {
	// Backtick code span: avoid breaking Markdown.
	return strings.ReplaceAll(strings.ReplaceAll(strings.TrimSpace(mname), "`", "'"), "\n", " ")
}

// formatOneRecentTrade renders one fill. rank1 is the global 1-based index across all fetched rows (stable across pages).
func formatOneRecentTrade(tr api.TradeHistoryItem, marketName func(string) string, rank1 int) string {
	if marketName == nil {
		marketName = func(addr string) string { return addr }
	}
	mname := strings.TrimSpace(marketName(tr.Market))
	if mname == "" {
		mname = tr.Market
	}
	ts := "—"
	if tr.TransactionUnixMs > 0 {
		ts = time.UnixMilli(tr.TransactionUnixMs).In(time.Local).Format("1/2 15:04")
	}
	sz := math.Abs(tr.Size)
	mktCode := tradeMarketForCode(mname)

	var b strings.Builder
	// Global numbering (page 2 starts at 6 when page size is 5). ZWSP avoids ordered-list parsing.
	b.WriteString(telegramLineNoListPrefix)
	b.WriteString(fmt.Sprintf("%d. ", rank1))
	// Title: action + market (code span keeps names readable even with slashes / underscores).
	b.WriteString("*" + tradeActionDisplay(tr.Action) + "* · `" + mktCode + "`\n")
	// Fill: price / size on one labeled row.
	b.WriteString(fmt.Sprintf("  成交价 `$%.4f` · 数量 `%.4f`\n", tr.Price, sz))
	// PnL + fees: one scan line.
	b.WriteString(fmt.Sprintf(
		"  盈亏 `$%.4f` · 资金费 `$%.4f` · 手续费 `$%.4f`\n",
		tr.RealizedPnlAmount, tr.RealizedFundingAmount, tr.FeeAmount,
	))
	// Meta: time only (source omitted in Telegram UI).
	b.WriteString(fmt.Sprintf("  时间 `%s`", escapeMarkdown(ts)))
	return b.String()
}

// formatRecentTrades renders paged trade_history for Telegram (Markdown).
// page is zero-based; refresh should pass 0.
func formatRecentTrades(items []api.TradeHistoryItem, page int, marketName func(string) string) string {
	if len(items) == 0 {
		return "*📜 最近成交*\n暂无记录。"
	}
	page = ClampTradesPage(page, len(items))
	tp := TradesTotalPages(len(items))
	start := page * TradesPageSize
	end := start + TradesPageSize
	if end > len(items) {
		end = len(items)
	}

	var sb strings.Builder
	sb.WriteString(tradesHeadingLine(page+1, tp))
	sb.WriteString("\n\n")
	for i := start; i < end; i++ {
		if i > start {
			sb.WriteString("\n" + tradeCardSeparator + "\n")
		}
		sb.WriteString(formatOneRecentTrade(items[i], marketName, i+1))
	}
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

func formatHelp(cfg Config) string {
	var sb strings.Builder
	sb.WriteString("*🤖 Decibel 做市机器人*\n\n")
	sb.WriteString("*可用命令*\n")
	sb.WriteString("/balance — 查看账户余额\n")
	sb.WriteString("/gas — 查看钱包 APT 余额\n")
	sb.WriteString("/positions — 查看当前仓位\n")
	sb.WriteString("/trades — 最近成交（每页 5 条，可翻页）\n")
	sb.WriteString("/help — 显示帮助\n")
	sb.WriteString("下方按钮可快捷打开对应视图（与命令等价）。\n\n")

	sb.WriteString("*Telegram 配置*\n")
	sb.WriteString("启用 bot 需同时设置 `TG_BOT_TOKEN` 与 `TG_ADMIN_ID`（环境变量或 `--tg-token` / `--tg-admin-id`）。\n")
	sb.WriteString("凭证优先用 `.env`；命令行传参会出现在进程列表。\n")
	sb.WriteString("库存告警：`TG_ALERT_INVENTORY`（或 `--tg-alert-inventory`）\n")
	sb.WriteString("告警间隔（分钟）：`TG_ALERT_INVENTORY_INTERVAL_MIN`（或 `--tg-alert-interval`）\n")
	sb.WriteString("严格启动：`TG_STRICT_START`（或 `--tg-strict-start`）— Telegram 就绪失败则进程退出。\n\n")

	alertLine := "仓位超限提醒: 关闭"
	if cfg.AlertInventory {
		alertLine = fmt.Sprintf("仓位超限提醒: 开启（每 %d 分钟检查）", cfg.AlertInventoryInterval)
	}
	sb.WriteString("*当前进程*\n")
	sb.WriteString(alertLine)
	sb.WriteString("\n\n")

	sb.WriteString("*安全说明*\n")
	sb.WriteString("仅在私聊中与配置的 admin 生效；群组内不响应。\n")

	return sb.String()
}

// ── Internal helpers ─────────────────────────────────────────────────────────

func cycleAge(lastCycleAt time.Time) string {
	if lastCycleAt.IsZero() {
		return "正在获取..."
	}
	t := lastCycleAt.Local()
	now := time.Now().In(t.Location())
	if t.Year() == now.Year() {
		return "更新于 " + t.Format("1/2 15:04")
	}
	return "更新于 " + t.Format("2006/1/2 15:04")
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
