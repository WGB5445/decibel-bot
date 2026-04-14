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
	"decibel-mm-bot/i18n"
)

// ── Balance ──────────────────────────────────────────────────────────────────

func formatBalance(tr *i18n.Telegram, snap botstate.Snapshot) string {
	available := snap.Equity * (1.0 - snap.MarginUsage)
	return fmt.Sprintf(tr.BalanceFmt,
		available, snap.Equity, snap.MarginUsage*100, cycleAge(tr, snap.LastCycleAt),
	)
}

// ── Gas ──────────────────────────────────────────────────────────────────────

func formatGas(tr *i18n.Telegram, walletAddr string, aptBal float64, err error) string {
	if err != nil {
		return fmt.Sprintf(tr.GasErrFmt, walletAddr, err)
	}
	return fmt.Sprintf(tr.GasFmt, walletAddr, aptBal)
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
	sort.SliceStable(out, func(i, j int) bool {
		ni := api.NormalizeAddr(out[i].MarketID)
		nj := api.NormalizeAddr(out[j].MarketID)
		if ni != nj {
			return ni < nj
		}
		return out[i].MarketID < out[j].MarketID
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
func markdownHeadingWithPage(tr *i18n.Telegram, leftBold string, page1Based, totalPages int) string {
	right := fmt.Sprintf(tr.PageFmt, page1Based, totalPages)
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
func positionsHeadingLine(tr *i18n.Telegram, page1Based, totalPages int) string {
	return markdownHeadingWithPage(tr, tr.PositionsTitleBold, page1Based, totalPages)
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

func positionDirectionDisplay(tr *i18n.Telegram, size float64) string {
	if size < 0 {
		return tr.PosShort
	}
	return tr.PosLong
}

func positionPnLLabel(tr *i18n.Telegram, pnl float64) string {
	if pnl < 0 {
		return tr.PnLLoss
	}
	return tr.PnLProfit
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
func formatPositions(tr *i18n.Telegram, snap botstate.Snapshot, page int, marketName func(string) string) string {
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
		sb.WriteString(positionsHeadingLine(tr, 1, 1))
		sb.WriteString("\n\n")
		sb.WriteString(tr.PosEmpty + "\n")
		sb.WriteString(positionsEmptySpacer())
		sb.WriteString(fmt.Sprintf("\n_%s_", cycleAge(tr, snap.LastCycleAt)))
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
	sb.WriteString(positionsHeadingLine(tr, page+1, tp))
	sb.WriteString("\n\n")

	for i, p := range list[start:end] {
		if i > 0 {
			sb.WriteString(tr.PosSep + "\n")
		}
		label := marketName(p.MarketID)
		markMid, hasMid := positionMid(snap, p.MarketID)
		notional, hasNotional := positionNotional(p, markMid, hasMid)
		dir := positionDirectionDisplay(tr, p.Size)
		sb.WriteString(fmt.Sprintf("• *%s* · *%s*\n", escapeMarkdown(label), dir))
		sb.WriteString(fmt.Sprintf(tr.SizeLevFmt, math.Abs(p.Size), p.UserLeverage))

		switch {
		case p.EntryPrice > 0 && hasMid:
			sb.WriteString(fmt.Sprintf(tr.EntryMidFmt, p.EntryPrice, markMid))
		case p.EntryPrice > 0:
			sb.WriteString(fmt.Sprintf(tr.EntryDashFmt, p.EntryPrice))
		case hasMid:
			sb.WriteString(fmt.Sprintf(tr.MidOnlyFmt, markMid))
		}

		switch {
		case hasNotional && hasMid:
			sb.WriteString(fmt.Sprintf(tr.NotionalFmt, notional))
			pnl, pct, ok := positionPnL(p, markMid)
			if ok {
				sb.WriteString(fmt.Sprintf(tr.PnLEstFmt, positionPnLLabel(tr, pnl), formatPnL(pnl, pct)))
			} else {
				sb.WriteString(tr.PnLNA)
			}
		case hasNotional:
			sb.WriteString(fmt.Sprintf(tr.NotionalFmt, notional))
			sb.WriteString(tr.PnLNA)
		case hasMid:
			if pnl, pct, ok := positionPnL(p, markMid); ok {
				sb.WriteString(fmt.Sprintf(tr.PnLEstFmt, positionPnLLabel(tr, pnl), formatPnL(pnl, pct)))
			} else {
				sb.WriteString(tr.PnLNA)
			}
		}

		var extras []string
		if p.UnrealizedFunding != 0 {
			extras = append(extras, fmt.Sprintf(tr.FundingFmt, p.UnrealizedFunding))
		}
		if p.EstimatedLiquidationPrice > 0 {
			extras = append(extras, fmt.Sprintf(tr.LiqFmt, p.EstimatedLiquidationPrice))
		}
		if len(extras) > 0 {
			sb.WriteString("  " + strings.Join(extras, " · ") + "\n")
		}
	}
	sb.WriteString(fmt.Sprintf("\n_%s_", cycleAge(tr, snap.LastCycleAt)))
	return sb.String()
}

// telegramPreBlock wraps body in a Markdown pre block (```) so the user can
// long-press and copy one contiguous region. Body must not contain raw ```.
func telegramPreBlock(body string) string {
	body = strings.ReplaceAll(body, "```", "``\u200d``")
	return "```\n" + body + "\n```"
}

// formatTradeFromHistory formats one trade_history row after a successful flatten.
func formatTradeFromHistory(tr *i18n.Telegram, trd api.TradeHistoryItem, marketName, txHash string) string {
	ts := tr.EmDash
	if trd.TransactionUnixMs > 0 {
		ts = time.UnixMilli(trd.TransactionUnixMs).In(time.Local).Format("2006-01-02 15:04:05")
	}
	action := tradeActionDisplay(tr, trd.Action)
	var sb strings.Builder
	sb.WriteString(tr.TradeFilledTitle)
	sb.WriteString(fmt.Sprintf(tr.TradeLineMarket, escapeMarkdown(marketName)))
	sb.WriteString(fmt.Sprintf(tr.TradeLineAction, action))
	sb.WriteString(fmt.Sprintf(tr.TradePxQtyFmt, trd.Price, trd.Size))
	sb.WriteString(fmt.Sprintf(tr.TradeRealFmt, trd.RealizedPnlAmount))
	sb.WriteString(fmt.Sprintf(tr.TradeFeeFmt, trd.RealizedFundingAmount, trd.FeeAmount))
	sb.WriteString(fmt.Sprintf(tr.TradeTimeFmt, escapeMarkdown(ts)))
	sb.WriteString(tr.TradeTxLine)
	sb.WriteString(telegramPreBlock(strings.TrimSpace(txHash)))
	sb.WriteString(tr.TradeResultFootnote)
	return sb.String()
}

// formatFlattenSubmittedNoHistory is shown when the chain accepted the order but
// REST has no matching trade row yet (or order_id unknown).
func formatFlattenSubmittedNoHistory(tr *i18n.Telegram, snap botstate.Snapshot, txHash, orderID string, reason string) string {
	midStr := tr.MarketPriceWord
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
	return fmt.Sprintf(tr.FlattenSubmittedTitleFmt, midStr) +
		tr.FlattenSubmittedTx +
		telegramPreBlock(strings.TrimSpace(txHash)) + extra + "\n" +
		tr.FlattenSubmittedHint +
		tr.FlattenSubmittedFoot +
		fmt.Sprintf("_%s_", cycleAge(tr, snap.LastCycleAt))
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

// tradeActionDisplay maps API action strings to short labels for Telegram.
func tradeActionDisplay(tr *i18n.Telegram, action string) string {
	switch normTradeAction(action) {
	case "closelong":
		return tr.TradeCloseLong
	case "closeshort":
		return tr.TradeCloseShort
	case "openlong":
		return tr.TradeOpenLong
	case "openshort":
		return tr.TradeOpenShort
	default:
		a := strings.TrimSpace(action)
		if a == "" {
			return tr.EmDash
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

func tradesHeadingLine(tr *i18n.Telegram, page1Based, totalPages int) string {
	return markdownHeadingWithPage(tr, tr.TradesTitleBold, page1Based, totalPages)
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
func formatOneRecentTrade(tr *i18n.Telegram, trd api.TradeHistoryItem, marketName func(string) string, rank1 int) string {
	if marketName == nil {
		marketName = func(addr string) string { return addr }
	}
	mname := strings.TrimSpace(marketName(trd.Market))
	if mname == "" {
		mname = trd.Market
	}
	ts := tr.EmDash
	if trd.TransactionUnixMs > 0 {
		ts = time.UnixMilli(trd.TransactionUnixMs).In(time.Local).Format("1/2 15:04")
	}
	sz := math.Abs(trd.Size)
	mktCode := tradeMarketForCode(mname)

	var b strings.Builder
	b.WriteString(telegramLineNoListPrefix)
	b.WriteString(fmt.Sprintf("%d. ", rank1))
	b.WriteString("*" + tradeActionDisplay(tr, trd.Action) + "* · `" + mktCode + "`\n")
	b.WriteString(fmt.Sprintf(tr.RecentPxQtyFmt, trd.Price, sz))
	b.WriteString(fmt.Sprintf(
		tr.RecentPnLLineFmt,
		trd.RealizedPnlAmount, trd.RealizedFundingAmount, trd.FeeAmount,
	))
	b.WriteString(fmt.Sprintf(tr.RecentTimeFmt, escapeMarkdown(ts)))
	return b.String()
}

// formatRecentTrades renders paged trade_history for Telegram (Markdown).
// page is zero-based; refresh should pass 0.
func formatRecentTrades(tr *i18n.Telegram, items []api.TradeHistoryItem, page int, marketName func(string) string) string {
	if len(items) == 0 {
		return tr.RecentTradesEmpty
	}
	page = ClampTradesPage(page, len(items))
	tp := TradesTotalPages(len(items))
	start := page * TradesPageSize
	end := start + TradesPageSize
	if end > len(items) {
		end = len(items)
	}

	var sb strings.Builder
	sb.WriteString(tradesHeadingLine(tr, page+1, tp))
	sb.WriteString("\n\n")
	for i := start; i < end; i++ {
		if i > start {
			sb.WriteString("\n" + tradeCardSeparator + "\n")
		}
		sb.WriteString(formatOneRecentTrade(tr, items[i], marketName, i+1))
	}
	return sb.String()
}

// ── Inventory alert ──────────────────────────────────────────────────────────

func formatInventoryAlert(tr *i18n.Telegram, snap botstate.Snapshot, maxInventory float64) string {
	dir := tr.InvSideLong
	if snap.Inventory < 0 {
		dir = tr.InvSideShort
	}
	midStr := "N/A"
	if snap.Mid != nil {
		midStr = fmt.Sprintf("$%.2f", *snap.Mid)
	}

	var pnlLine string
	if snap.EntryPrice > 0 && snap.Mid != nil {
		pnl := (*snap.Mid - snap.EntryPrice) * snap.Inventory
		pct := (*snap.Mid - snap.EntryPrice) / snap.EntryPrice * 100
		pnlLine = fmt.Sprintf(tr.InvPnLPrefixFmt+"%s", formatPnL(pnl, pct))
	}

	return fmt.Sprintf(
		tr.InvAlertTitle+
			tr.InvAlertMarketFmt+
			tr.InvAlertSideFmt+
			tr.InvAlertPosFmt+
			tr.InvAlertMidFmt+"%s",
		escapeMarkdown(snap.TargetMarketName), dir,
		math.Abs(snap.Inventory), maxInventory,
		midStr, pnlLine,
	)
}

// ── Help ─────────────────────────────────────────────────────────────────────

func formatHelp(cfg Config, tr *i18n.Telegram) string {
	var sb strings.Builder
	sb.WriteString(tr.HelpTitle)
	sb.WriteString(tr.HelpCmdHeader)
	sb.WriteString(tr.HelpCmdBalance)
	sb.WriteString(tr.HelpCmdGas)
	sb.WriteString(tr.HelpCmdPositions)
	sb.WriteString(tr.HelpCmdTrades)
	sb.WriteString(tr.HelpCmdHelp)
	sb.WriteString(tr.HelpButtonsHint)

	sb.WriteString(tr.HelpTgHeader)
	sb.WriteString(tr.HelpTgBody)

	alertLine := tr.HelpAlertOff
	if cfg.AlertInventory {
		alertLine = fmt.Sprintf(tr.HelpAlertOnFmt, cfg.AlertInventoryInterval)
	}
	sb.WriteString(tr.HelpProcessHeader)
	sb.WriteString(alertLine)
	sb.WriteString("\n\n")

	sb.WriteString(tr.HelpSecurityHeader)
	sb.WriteString(tr.HelpSecurityBody)

	return sb.String()
}

// ── Internal helpers ─────────────────────────────────────────────────────────

func cycleAge(tr *i18n.Telegram, lastCycleAt time.Time) string {
	if lastCycleAt.IsZero() {
		return tr.CycleAgeFetching
	}
	t := lastCycleAt.Local()
	now := time.Now().In(t.Location())
	if t.Year() == now.Year() {
		return tr.CycleAgeUpdatedPrefix + t.Format("1/2 15:04:05")
	}
	return tr.CycleAgeUpdatedPrefix + t.Format("2006/1/2 15:04:05")
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
