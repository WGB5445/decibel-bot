package i18n

// Telegram holds all user-visible strings for the Telegram bot (Markdown where applicable).
type Telegram struct {
	BtnRefresh, BtnBack                         string
	BtnBalance, BtnGas, BtnPositions, BtnTrades string
	BtnMarketClose                              string
	BtnCloseMarketFmt                           string // printf, one %s = market display name
	ErrRefreshFmt                               string // %v
	ErrQueryRetryFmt                            string // %v
	ErrQueryFmt                                 string // %v
	Flattening                                  string
	FlattenNoNeed                               string
	FlattenFailFmt                              string // %v
	FlattenPosRefreshFailFmt                    string
	FlattenInvRefreshFailFmt                    string
	PositionRecovered                           string
	DryRunNoHistory                             string
	FlattenQueryFailFmt                         string // tx, orderID, err
	ReasonOrderIDParse                          string
	ReasonTradePending                          string
	MarketPriceWord                             string // mark/mid label when no numeric mid in flatten summary
	PageFmt                                     string // two ints, produces italic page e.g. _第 %d/%d 页_
	PositionsTitleBold                          string // includes * for markdown
	TradesTitleBold                             string
	PosEmpty                                    string
	PosLong, PosShort                           string
	PnLProfit, PnLLoss                          string
	PosSep                                      string
	SizeLevFmt                                  string
	EntryMidFmt                                 string
	EntryDashFmt                                string
	MidOnlyFmt                                  string
	NotionalFmt                                 string
	PnLEstFmt                                   string // label (profit/loss word), formatPnL result
	PnLNA                                       string
	FundingFmt                                  string
	LiqFmt                                      string
	TradeFilledTitle                            string
	TradeLineMarket                             string
	TradeLineAction                             string
	TradePxQtyFmt                               string
	TradeRealFmt                                string
	TradeFeeFmt                                 string
	TradeTimeFmt                                string
	TradeTxLine                                 string
	TradeResultFootnote                         string
	FlattenSubmittedTitleFmt                    string
	FlattenSubmittedTx                          string // "tx:\n"
	FlattenSubmittedHint                        string
	FlattenSubmittedFoot                        string
	TradeCloseLong                              string
	TradeCloseShort                             string
	TradeOpenLong                               string
	TradeOpenShort                              string
	EmDash                                      string
	RecentTradesEmpty                           string
	RecentPxQtyFmt                              string
	RecentPnLLineFmt                            string
	RecentTimeFmt                               string
	InvAlertTitle                               string
	InvAlertMarketFmt                           string
	InvAlertSideFmt                             string
	InvAlertPosFmt                              string
	InvAlertMidFmt                              string
	InvSideLong, InvSideShort                   string
	InvPnLPrefixFmt                             string // starts with \n~
	BalanceFmt                                  string // avail, equity, marginPct, cycleAge
	GasErrFmt                                   string
	GasFmt                                      string
	HelpTitle                                   string
	HelpCmdHeader                               string
	HelpCmdBalance                              string
	HelpCmdGas                                  string
	HelpCmdPositions                            string
	HelpCmdTrades                               string
	HelpCmdHelp                                 string
	HelpButtonsHint                             string
	HelpTgHeader                                string
	HelpTgBody                                  string
	HelpAlertOff                                string
	HelpAlertOnFmt                              string // one %d minutes
	HelpProcessHeader                           string
	HelpSecurityHeader                          string
	HelpSecurityBody                            string
	CycleAgeFetching                            string
	CycleAgeUpdatedPrefix                       string // + time in code
	CmdBalanceDesc                              string
	CmdGasDesc                                  string
	CmdPositionsDesc                            string
	CmdTradeHistDesc                            string
	CmdHelpDesc                                 string
}

// Bundle returns the Telegram string table for the given locale.
func Bundle(loc Locale) *Telegram {
	if loc == LocaleEN {
		return &telegramEN
	}
	return &telegramZH
}

var telegramZH = Telegram{
	BtnRefresh: "🔄 刷新", BtnBack: "🔙 返回",
	BtnBalance: "💰 余额", BtnGas: "⛽ Gas", BtnPositions: "📊 仓位", BtnTrades: "📜 成交",
	BtnMarketClose: "❌ 市价平仓", BtnCloseMarketFmt: "❌ 平仓 %s",
	ErrRefreshFmt: "刷新失败: %v", ErrQueryRetryFmt: "查询失败，请稍后重试: %v", ErrQueryFmt: "查询失败: %v",
	Flattening: "正在平仓", FlattenNoNeed: "ℹ️ 当前目标市场无仓位或仓位过小，无需重复平仓。",
	FlattenFailFmt: "❌ 平仓失败: %v", FlattenPosRefreshFailFmt: "平仓已提交但刷新仓位失败: %v",
	FlattenInvRefreshFailFmt: "平仓已提交但刷新失败: %v",
	PositionRecovered:        "✅ 仓位已恢复正常范围。",
	DryRunNoHistory:          "*ℹ️ 模拟运行*\n未提交链上交易，无法查询成交历史。",
	FlattenQueryFailFmt:      "✅ 平仓单已提交\ntx: %s\norder_id: %s\n查询成交失败: %v",
	ReasonOrderIDParse:       "未能从链上事件解析 order_id", ReasonTradePending: "成交历史暂未索引到该订单",
	MarketPriceWord: "市价", PageFmt: "_第 %d/%d 页_",
	PositionsTitleBold: "*📊 当前仓位*", TradesTitleBold: "*📜 最近成交*",
	PosEmpty: "_暂无持仓_", PosLong: "多 ▲", PosShort: "空 ▼", PnLProfit: "盈利", PnLLoss: "亏损",
	PosSep:      "  ──────────────",
	SizeLevFmt:  "  持仓 `%.5f` · 杠杆 `%.0fx`\n",
	EntryMidFmt: "  开仓价 `$%.2f` · 当前价 `$%.2f`\n", EntryDashFmt: "  开仓价 `$%.2f` · 当前价 `—`\n",
	MidOnlyFmt: "  当前价 `$%.2f`\n", NotionalFmt: "  持仓价值 `$%.2f`\n",
	PnLEstFmt: "  %s(估算) %s\n", PnLNA: "  盈亏(估算) `—`\n",
	FundingFmt: "资金费 `$%.4f`", LiqFmt: "强平 `$%.2f`",
	TradeFilledTitle: "*✅ 平仓成交*\n\n", TradeLineMarket: "• *%s*\n", TradeLineAction: "  *%s*\n",
	TradePxQtyFmt: "  成交价 `$%.4f` · 数量 `%.4f`\n", TradeRealFmt: "  实现盈亏 `$%.4f`\n",
	TradeFeeFmt: "  资金费 `$%.4f` · 手续费 `$%.4f`\n", TradeTimeFmt: "  时间 `%s`\n",
	TradeTxLine: "tx:\n", TradeResultFootnote: "\n_本条为平仓结果，不再更新。点「刷新」在下方新消息查看仓位。_",
	FlattenSubmittedTitleFmt: "*✅ 平仓单已提交* (~%s)\n", FlattenSubmittedTx: "tx:\n",
	FlattenSubmittedHint: "请稍后点「刷新」在新消息查看仓位，或点「成交」查看历史。\n",
	FlattenSubmittedFoot: "_本条为平仓结果，不再更新。_\n",
	TradeCloseLong:       "平多", TradeCloseShort: "平空", TradeOpenLong: "开多", TradeOpenShort: "开空", EmDash: "—",
	RecentTradesEmpty: "*📜 最近成交*\n暂无记录。",
	RecentPxQtyFmt:    "  成交价 `$%.4f` · 数量 `%.4f`\n",
	RecentPnLLineFmt:  "  盈亏 `$%.4f` · 资金费 `$%.4f` · 手续费 `$%.4f`\n",
	RecentTimeFmt:     "  时间 `%s`",
	InvAlertTitle:     "⚠️ *仓位超限提醒*\n", InvAlertMarketFmt: "市场: `%s`\n", InvAlertSideFmt: "方向: `%s`\n",
	InvAlertPosFmt: "仓位: `%.5f` (限制: `%.5f`)", InvAlertMidFmt: "\n当前价: `%s`",
	InvSideLong: "LONG ▲", InvSideShort: "SHORT ▼", InvPnLPrefixFmt: "\n~盈亏: ",
	BalanceFmt: "*💰 账户余额*\n可用余额: `$%.2f`\n总权益: `$%.2f`\n保证金占用: `%.1f%%`\n_%s_",
	GasErrFmt:  "*⛽ Gas 钱包*\n地址: `%s`\n❌ 查询失败: %v", GasFmt: "*⛽ Gas 钱包*\n地址: `%s`\nAPT 余额: `%.4f APT`",
	HelpTitle: "*🤖 Decibel 做市机器人*\n\n", HelpCmdHeader: "*可用命令*\n",
	HelpCmdBalance: "/balance — 查看账户余额\n", HelpCmdGas: "/gas — 查看钱包 APT 余额\n",
	HelpCmdPositions: "/positions — 查看当前仓位\n", HelpCmdTrades: "`/trade_history` — 最近成交（每页 5 条，可翻页）\n",
	HelpCmdHelp:     "/help — 显示帮助\n",
	HelpButtonsHint: "下方按钮可快捷打开对应视图（与命令等价）。\n\n",
	HelpTgHeader:    "*Telegram 配置*\n",
	HelpTgBody: "启用 bot 需同时设置 `TG_BOT_TOKEN` 与 `TG_ADMIN_ID`（环境变量或 `--tg-token` / `--tg-admin-id`）。\n" +
		"凭证优先用 `.env`；命令行传参会出现在进程列表。\n" +
		"库存告警：`TG_ALERT_INVENTORY`（或 `--tg-alert-inventory`）\n" +
		"告警间隔（分钟）：`TG_ALERT_INVENTORY_INTERVAL_MIN`（或 `--tg-alert-interval`）\n" +
		"严格启动：`TG_STRICT_START`（或 `--tg-strict-start`）— Telegram 就绪失败则进程退出。\n\n",
	HelpAlertOff: "仓位超限提醒: 关闭", HelpAlertOnFmt: "仓位超限提醒: 开启（每 %d 分钟检查）",
	HelpProcessHeader: "*当前进程*\n", HelpSecurityHeader: "*安全说明*\n",
	HelpSecurityBody: "仅在私聊中与配置的 admin 生效；群组内不响应。\n",
	CycleAgeFetching: "正在获取...", CycleAgeUpdatedPrefix: "更新于 ",
	CmdBalanceDesc: "查看账户余额", CmdGasDesc: "查看钱包 APT 余额", CmdPositionsDesc: "查看当前仓位",
	CmdTradeHistDesc: "成交历史 trade_history（每页5条，可翻页）", CmdHelpDesc: "显示帮助",
}

var telegramEN = Telegram{
	BtnRefresh: "🔄 Refresh", BtnBack: "🔙 Back",
	BtnBalance: "💰 Balance", BtnGas: "⛽ Gas", BtnPositions: "📊 Positions", BtnTrades: "📜 Trades",
	BtnMarketClose: "❌ Market close", BtnCloseMarketFmt: "❌ Close %s",
	ErrRefreshFmt: "Refresh failed: %v", ErrQueryRetryFmt: "Query failed, try again later: %v", ErrQueryFmt: "Query failed: %v",
	Flattening: "Closing position…", FlattenNoNeed: "ℹ️ No position (or too small) on the target market; nothing to close.",
	FlattenFailFmt: "❌ Close failed: %v", FlattenPosRefreshFailFmt: "Close submitted but refreshing positions failed: %v",
	FlattenInvRefreshFailFmt: "Close submitted but refresh failed: %v",
	PositionRecovered:        "✅ Position is back within limits.",
	DryRunNoHistory:          "*ℹ️ Dry run*\nNo on-chain txs; trade history is unavailable.",
	FlattenQueryFailFmt:      "✅ Close order submitted\ntx: %s\norder_id: %s\nFailed to load fills: %v",
	ReasonOrderIDParse:       "Could not parse order_id from chain events", ReasonTradePending: "Trade history not indexed for this order yet",
	MarketPriceWord: "Mark", PageFmt: "_Page %d/%d_",
	PositionsTitleBold: "*📊 Positions*", TradesTitleBold: "*📜 Recent trades*",
	PosEmpty: "_No open positions_", PosLong: "Long ▲", PosShort: "Short ▼", PnLProfit: "Profit", PnLLoss: "Loss",
	PosSep:      "  ──────────────",
	SizeLevFmt:  "  Size `%.5f` · Lev `%.0fx`\n",
	EntryMidFmt: "  Entry `$%.2f` · Mark `$%.2f`\n", EntryDashFmt: "  Entry `$%.2f` · Mark `—`\n",
	MidOnlyFmt: "  Mark `$%.2f`\n", NotionalFmt: "  Notional `$%.2f`\n",
	PnLEstFmt: "  %s (est.) %s\n", PnLNA: "  PnL (est.) `—`\n",
	FundingFmt: "Funding `$%.4f`", LiqFmt: "Liq. `$%.2f`",
	TradeFilledTitle: "*✅ Close filled*\n\n", TradeLineMarket: "• *%s*\n", TradeLineAction: "  *%s*\n",
	TradePxQtyFmt: "  Price `$%.4f` · Size `%.4f`\n", TradeRealFmt: "  Realized PnL `$%.4f`\n",
	TradeFeeFmt: "  Funding `$%.4f` · Fee `$%.4f`\n", TradeTimeFmt: "  Time `%s`\n",
	TradeTxLine: "tx:\n", TradeResultFootnote: "\n_This is the final close summary. Tap Refresh below to open a new positions message._",
	FlattenSubmittedTitleFmt: "*✅ Close order submitted* (~%s)\n", FlattenSubmittedTx: "tx:\n",
	FlattenSubmittedHint: "Tap Refresh in a new message for positions, or Trades for history.\n",
	FlattenSubmittedFoot: "_This close summary will not update._\n",
	TradeCloseLong:       "Close long", TradeCloseShort: "Close short", TradeOpenLong: "Open long", TradeOpenShort: "Open short", EmDash: "—",
	RecentTradesEmpty: "*📜 Recent trades*\nNo fills yet.",
	RecentPxQtyFmt:    "  Price `$%.4f` · Size `%.4f`\n",
	RecentPnLLineFmt:  "  PnL `$%.4f` · Funding `$%.4f` · Fee `$%.4f`\n",
	RecentTimeFmt:     "  Time `%s`",
	InvAlertTitle:     "⚠️ *Position limit*\n", InvAlertMarketFmt: "Market: `%s`\n", InvAlertSideFmt: "Side: `%s`\n",
	InvAlertPosFmt: "Size: `%.5f` (limit: `%.5f`)", InvAlertMidFmt: "\nMark: `%s`",
	InvSideLong: "LONG ▲", InvSideShort: "SHORT ▼", InvPnLPrefixFmt: "\n~PnL: ",
	BalanceFmt: "*💰 Account*\nAvailable: `$%.2f`\nEquity: `$%.2f`\nMargin usage: `%.1f%%`\n_%s_",
	GasErrFmt:  "*⛽ Gas wallet*\nAddress: `%s`\n❌ Query failed: %v", GasFmt: "*⛽ Gas wallet*\nAddress: `%s`\nAPT balance: `%.4f APT`",
	HelpTitle: "*🤖 Decibel market-maker*\n\n", HelpCmdHeader: "*Commands*\n",
	HelpCmdBalance: "/balance — account balance\n", HelpCmdGas: "/gas — wallet APT balance\n",
	HelpCmdPositions: "/positions — open positions\n", HelpCmdTrades: "`/trade_history` — recent fills (5 per page)\n",
	HelpCmdHelp:     "/help — this help\n",
	HelpButtonsHint: "Use the buttons below for the same actions.\n\n",
	HelpTgHeader:    "*Telegram setup*\n",
	HelpTgBody: "Set `TG_BOT_TOKEN` and `TG_ADMIN_ID` (env or `--tg-token` / `--tg-admin-id`).\n" +
		"Prefer `.env` for secrets; CLI args appear in the process list.\n" +
		"Inventory alert: `TG_ALERT_INVENTORY` (or `--tg-alert-inventory`)\n" +
		"Alert interval (minutes): `TG_ALERT_INVENTORY_INTERVAL_MIN` (or `--tg-alert-interval`)\n" +
		"Strict start: `TG_STRICT_START` (or `--tg-strict-start`) — exit if Telegram init fails.\n\n",
	HelpAlertOff: "Inventory alert: off", HelpAlertOnFmt: "Inventory alert: on (every %d min)",
	HelpProcessHeader: "*This process*\n", HelpSecurityHeader: "*Security*\n",
	HelpSecurityBody: "Only private chats with the configured admin; no replies in groups.\n",
	CycleAgeFetching: "Fetching…", CycleAgeUpdatedPrefix: "Updated ",
	CmdBalanceDesc: "Account balance", CmdGasDesc: "Wallet APT balance", CmdPositionsDesc: "Open positions",
	CmdTradeHistDesc: "Recent fills (5 per page)", CmdHelpDesc: "Show help",
}
