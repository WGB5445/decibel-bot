# AGENTS.md — Decibel Market Maker Bot (Go)

> 本文档面向 AI 编程助手。如果你是人类开发者，请先阅读 `README.md` 获取快速入门信息；本文档补充了 AI 助手在修改代码时必须了解的架构细节、状态机、事件流和编码约定。

---

## 1. 项目概述

**Decibel Market Maker Bot** 是一个用 Go 编写的自动化做市机器人，运行在 [Decibel](https://docs.decibel.trade/) 上——一个构建在 **Aptos 区块链** 上的全链永续 DEX。

核心能力：
- 通过 `aptos-go-sdk` 进行**链上订单下单**（单笔 + Bulk Order 原子批量更新）
- **REST API** 查询市场数据、仓位、订单状态
- **WebSocket 实时流**接收订单簿、价格、账户更新
- **Hummingbot 风格的永续做市策略**：双边报价、多档挂单、订单刷新容差、止盈止损、Triple Barrier（止损/止盈/时限/追踪止损）
- **交易追踪器**异步轮询 Aptos 交易确认
- **Prometheus 指标**暴露（`:2112/metrics`）

---

## 2. 技术栈

| 组件 | 依赖 |
|------|------|
| Go 版本 | `1.26.1` |
| Aptos SDK | `github.com/aptos-labs/aptos-go-sdk v1.12.1` |
| CLI | `github.com/urfave/cli/v2` |
| 配置 | `github.com/spf13/viper` + `github.com/go-playground/validator/v10` |
| WebSocket | `github.com/gorilla/websocket` |
| 精度计算 | `github.com/shopspring/decimal`（金融级十进制，避免浮点误差） |
| 日志 | `go.uber.org/zap` |
| 限流 | `golang.org/x/time/rate` |
| 指标 | `github.com/prometheus/client_golang` |

**关键约定**：所有价格和数量均使用 `decimal.Decimal`，链上交互时通过 `scalePrice`/`scaleSize` 转为 9 位小数的整数（uint64）。

---

## 3. 项目结构

```
decibel-mm-go/
├── cmd/
│   ├── decibel-bot/          # 主机器人入口
│   └── market-info/          # 辅助工具：列出市场和价格
├── internal/
│   ├── config/               # 配置加载、验证、默认值
│   ├── decibel/              # Decibel 客户端层
│   │   ├── aptos_client.go   # Aptos 节点封装（交易构建/签名/提交/等待）
│   │   ├── constants.go      # 合约地址、Move 函数名、TIF 枚举、精度常量
│   │   ├── read_client.go    # REST API 只读客户端（ markets/prices/depth/orders/positions ）
│   │   ├── write_client.go   # 链上写客户端（下单/撤单/批量单/出入金/配置杠杆）
│   │   ├── ws_client.go      # WebSocket 连接管理、自动重连、消息解析
│   │   ├── types.go          # REST/WS 相关的 DTO 类型
│   │   └── order_events.go   # 从交易事件解析 order_id
│   ├── engine/               # 核心引擎层
│   │   ├── eventbus.go       # 内存发布-订阅事件总线
│   │   ├── scheduler.go      # 定时产生 EventTick 驱动策略循环
│   │   ├── order_manager.go  # 本地订单影子状态管理
│   │   ├── position_manager.go # 本地仓位影子状态管理
│   │   ├── tx_tracker.go     # 待确认交易轮询追踪器
│   │   └── risk_guard.go     # 风险守卫（止损距离、爆仓接近度检查）
│   ├── models/               # 共享领域模型
│   │   ├── events.go         # EventType 枚举 + 所有事件结构体
│   │   ├── order.go          # LocalOrder、Side、OrderStatus
│   │   └── position.go       # LocalPosition
│   ├── strategy/             # 策略层
│   │   ├── decibel_mm.go     # 主策略状态机（DecibelMM）
│   │   ├── pricing.go        # 定价引擎（深度/市场价格聚合）
│   │   ├── proposal.go       # 订单提案构建器（多档、价格带、优化、过滤）
│   │   ├── barriers.go       # Triple Barrier（止损/止盈/时限/追踪止损）
│   │   └── state.go          # StrategyState 字符串表示
│   └── pkg/
│       ├── decimal/          # decimal 辅助（ToU64/FromU64/FromFloat/FromString）
│       ├── metrics/          # Prometheus 指标定义
│       ├── orderid/          # 订单 ID 生成/解析辅助
│       └── retry/            # 重试工具
├── configs/
│   └── config.yaml           # 示例配置文件
├── test/                     # Python Hummingbot 相关测试（遗留/参考用）
└── tests/integration/        # Go 集成测试（待补充）
```

---

## 4. 架构核心：事件驱动（Event-Driven）

机器人采用**纯事件驱动架构**。所有并发组件通过 `EventBus` 通信，避免共享状态锁地狱。

### 4.1 事件类型（EventType）

```go
const (
    EventTick          // 策略心跳（由 Scheduler 定时产生）
    EventDepthUpdate   // 订单簿深度更新
    EventPriceUpdate   // 市场价格更新（oracle/mark/mid）
    EventOrderUpdate   // 订单状态变化（NEW/PARTIALLY_FILLED/FILLED/CANCELLED）
    EventPositionUpdate // 仓位变化
    EventTradeFill     // 成交回报
    EventTxConfirmed   // 链上交易已确认
    EventTxFailed      // 链上交易失败或超时
)
```

### 4.2 事件流向

```
┌─────────────┐     ┌─────────────┐     ┌─────────────────┐
│  WS Client  │────▶│  EventBus   │◀────│   Scheduler     │
│ (外部数据流)  │     │  (pub-sub)  │     │ (定时 EventTick)│
└─────────────┘     └──────┬──────┘     └─────────────────┘
                           │
         ┌─────────────────┼─────────────────┐
         ▼                 ▼                 ▼
   ┌──────────┐    ┌─────────────┐    ┌─────────────┐
   │OrderMgr  │    │PositionMgr  │    │ DecibelMM   │
   │(影子订单) │    │ (影子仓位)   │    │ (策略状态机) │
   └──────────┘    └─────────────┘    └─────────────┘
                                              │
                                              ▼
                                       ┌─────────────┐
                                       │ WriteClient │──▶ Aptos 链上
                                       │ (下单/撤单)  │
                                       └─────────────┘
                                              │
                                              ▼
                                       ┌─────────────┐
                                       │  TxTracker  │──▶ 轮询确认
                                       │(交易追踪器)  │     后回写 EventBus
                                       └─────────────┘
```

### 4.3 数据流详细说明

1. **WebSocket Client** 连接 Decibel WS 网关，订阅 `depth`、`market_price`、`order_updates`、`account_positions`、`user_trades` 五个频道。
2. 收到消息后解析为对应 `models.Event`，推入 `EventBus`。
3. **Scheduler** 每 `tickInterval`（默认 5s，可通过 `order_refresh_time` 调整）发布 `EventTick`。
4. **主事件循环**（在 `cmd/decibel-bot/main.go` 中）按顺序消费事件：
   - 先更新 `OrderManager` / `PositionManager` 的本地影子状态
   - 再调用 `strat.HandleEvent(ev)` 进入策略状态机
5. **DecibelMM** 在 `OnTick` 中根据当前 `StrategyState` 计算目标订单，通过 `WriteClient` 提交链上交易。
6. **TxTracker** 后台轮询交易哈希，确认后发布 `EventTxConfirmed` / `EventTxFailed` 回事件总线。

---

## 5. 策略状态机（StrategyState Machine）

策略核心在 `internal/strategy/decibel_mm.go` 的 `DecibelMM.OnTick()` 中。

### 5.1 状态定义

```go
StateInit           // 初始化：设置市场杠杆/保证金模式，然后转移到 NO_POSITION
StateNoPosition     // 无仓位：常规做市挂双边单
StateMaking         // 正在做市（有挂单但无持仓）
StatePositionManage // 持仓中：管理止盈止损、追踪止损、时限平仓
StateCooldown       // 成交后冷却期（filled_order_delay），避免过度交易
```

### 5.2 状态转移图

```
                    ┌──────────┐
                    │  StateInit │
                    └────┬─────┘
                         │ setupMarketSettings()
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  StateNoPosition / StateMaking                               │
│  ─────────────────────────────────────────────────────────  │
│  • 根据参考价创建 Proposal（多档 bid/ask）                    │
│  • ApplyPriceBand（价格天花板/地板过滤）                      │
│  • ApplyOrderOptimization（单档时跳价抢最优）                 │
│  • FilterOutTakers（防止挂单反吃）                           │
│  • cancelOrdersBelowMinSpread（低于最小价差则撤单）            │
│  • 若 use_bulk_orders=true：打包为 BulkOrder 一次性提交        │
│    否则：cancelStaleOrders + executeProposal（逐笔下单）       │
└─────────────────────────────────────────────────────────────┘
         │                              │
         │ 成交/开仓                     │ 全部平仓
         ▼                              ▼
   StatePositionManage            StateCooldown
   ─────────────────────         ─────────────────────
   • profitTakingProposal()       • 等待 filled_order_delay
   • TripleBarrier 检查：          • 结束后回到 NoPosition
     - StopLoss / TrailingStop   （若仍有仓位则到 PositionManage）
     - TakeProfit
     - TimeLimit
   • 触发任意 barrier → closeAllWithMarket()
```

### 5.3 Triple Barrier 逻辑

在 `internal/strategy/barriers.go` 中实现：

- **Stop Loss**：当 `pnlPct <= -stop_loss` 时触发
- **Take Profit**：当 `pnlPct >= take_profit` 时触发
- **Time Limit**：持仓时间超过 `time_limit` 时触发
- **Trailing Stop**：
  1. 当盈利达到 `activation_price` 时激活
  2. 记录当前盈利 - `trailing_delta` 作为触发线
  3. 盈利继续上涨时，触发线上移
  4. 一旦盈利回落跌破触发线，平仓

PnL 计算使用 `ComputePnLPct(pos, currentPrice)`：
- 多头：`(currentPrice - entryPrice) / entryPrice`
- 空头：`(entryPrice - currentPrice) / entryPrice`

---

## 6. 订单与仓位管理

### 6.1 OrderManager（影子订单）

- `active` map：以 `orderID`（或 `clientOrderID`）为 key 的 `LocalOrder` 集合
- `history` slice：已终结的订单记录
- 关键方法：
  - `AddPending`：提交链上交易后立即记录，状态为 `PENDING_NEW`
  - `ConfirmOrder`：交易确认后升级为 `NEW`
  - `UpdateFromWS`：根据 WS `order_updates` 更新 filled_size / avg_price / status
  - `ReplaceAllForMarket`：Bulk Order 提交后，替换该市场所有本地影子订单
  - `CancelOrder`：标记为 `CANCELLED` 并移入历史

### 6.2 PositionManager（影子仓位）

- `positions` map：以 `marketAddr` 为 key
- 支持两种更新来源：
  - `UpdateFromWS`：来自 `account_positions` WS 频道的全量推送
  - `UpdateFromTradeFill`：来自 `user_trades` 成交回报，增量更新仓位数量与均价
- 加仓时， entry_price 按**加权平均**重新计算：
  ```
  newEntry = (oldAmt * oldPrice + fillAmt * fillPrice) / (oldAmt + fillAmt)
  ```

---

## 7. 链上交互层（decibel 包）

### 7.1 环境配置

| 环境 | REST Base | WS Base | Fullnode | Package Address |
|------|-----------|---------|----------|-----------------|

| `testnet` | `api.testnet.aptoslabs.com/decibel` | `.../ws` | `api.testnet.aptoslabs.com/v1` | `0xe7da...1b7f` |
| `mainnet` | `api.mainnet.aptoslabs.com/decibel` | `.../ws` | `api.mainnet.aptoslabs.com/v1` | （待补充） |

### 7.2 WriteClient 链上操作

所有操作通过调用 Decibel Move 合约的 entry function 完成：

| 方法 | Move 函数 | 说明 |
|------|-----------|------|
| `PlaceOrder` | `dex_accounts_entry::place_order_to_subaccount` | 单笔下单 |
| `CancelOrder` | `dex_accounts_entry::cancel_order` | 单笔撤单 |
| `CancelAllOrders` | `dex_accounts_entry::cancel_all_orders` | 撤某市场全部订单 |
| `PlaceBulkOrders` | `dex_accounts_entry::place_bulk_orders_to_subaccount` | **批量原子下单** |
| `DepositUSDC` | `dex_accounts_entry::deposit_to_subaccount` | 存入保证金 |
| `WithdrawUSDC` | `dex_accounts_entry::withdraw_from_subaccount` | 提取保证金 |
| `ConfigureMarketSettings` | `dex_accounts_entry::configure_user_settings_for_market` | 设置杠杆/保证金模式 |

### 7.3 Bulk Order 序列号同步

`PlaceBulkOrders` 需要递增的 `sequence_number`（u64）。`WriteClient` 内部通过 `syncBulkSeqFromREST` 从 `GET /bulk_orders` 获取当前最大序列号作为起始点，随后内存自增。

- `bulkSeqSynced` 标记首次同步是否完成
- `ResetBulkSeq()` 可在异常后强制重新同步

### 7.4 地址处理规范

项目内使用两套辅助函数处理 Aptos 地址（`write_client.go`）：
- `NormalizeAddr(addr)`：去 `0x` 前缀、转小写、去前导零
- `AddrEqual(a, b)`：基于 NormalizeAddr 的比较
- `AddrSuffix(addr)`：取最后 6 位用于日志

---

## 8. 配置系统（config 包）

使用 **Viper** 加载 YAML，支持环境变量覆盖（前缀 `DECIBEL_`）。所有 `decimal.Decimal` 字段通过自定义 `mapstructure.DecodeHook` 自动从 string/float/int 解析。

### 8.1 关键配置项

```yaml
env: testnet                    # testnet | mainnet

decibel:
  bearer_token: ""              # Decibel API 认证令牌
  api_wallet_private_key: ""    # Aptos 钱包私钥（hex 或 AIP-80 格式）
  api_wallet_address: ""        # 钱包地址
  subaccount_address: ""        # 交易子账户地址
  market_name: "BTC-USD"        # 目标市场

strategy:
  leverage: 10                  # 杠杆倍数
  bid_spread: "0.01"            # 买价偏离中价的百分比
  ask_spread: "0.01"            # 卖价偏离中价的百分比
  order_amount: "0.1"           # 基础下单量
  order_levels: 1               # 每边挂单档数
  order_level_spread: "0.01"    # 档间价差
  order_refresh_time: "30s"     # 订单刷新间隔
  order_refresh_tolerance_pct: "0" # 价格变化低于此值不刷新
  filled_order_delay: "60s"     # 成交后冷却期
  minimum_spread: "-100"        # 最小价差（低于则撤单，-100=不限制）

  stop_loss: "0.03"             # 止损比例
  take_profit: "0.02"           # 止盈比例
  time_limit: "45m"             # 持仓时限

  use_bulk_orders: true         # 是否使用批量下单
  post_only: true               # 是否使用 PostOnly TIF
  max_pending_tx: 10            # 最大待确认交易数
  tx_poll_interval: "2s"        # 交易轮询间隔
```

### 8.2 验证规则

- `env` 必须为 `testnet` / `mainnet` 之一
- `bearer_token`, `api_wallet_private_key`, `api_wallet_address`, `subaccount_address`, `market_name` 均为 `required`

---

## 9. 编码约定与开发规范

### 9.1 Go 风格

- 使用标准 Go 命名：`CamelCase` 导出，`camelCase` 私有
- 错误处理：始终 `fmt.Errorf("context: %w", err)` 包装错误
- 上下文传递：所有阻塞/IO 函数接受 `ctx context.Context`
- 并发安全：共享状态使用 `sync.RWMutex`，优先通过 EventBus 传递事件而非直接共享
- 日志：使用 `zap`，结构化字段（`zap.String`, `zap.Error`, `zap.Int`）

### 9.2 数值处理

- **严禁**在价格和数量计算中使用 `float64`，统一使用 `github.com/shopspring/decimal`
- 链上提交前必须通过 `scalePrice` / `scaleSize` 将 decimal 转为 9 位小数的 uint64
- `quantize(d, places)` 函数用于按交易所精度截断价格/数量

### 9.3 测试

- 单元测试：`go test -v ./...`
- 测试文件命名：`*_test.go`
- 当前测试覆盖：config、eventbus、order_manager、position_manager、barriers、proposal、decimal、retry
- 集成测试目录：`tests/integration/`（待补充）

### 9.4 新增文件/包时的 checklist

1. 是否需要在 `models/` 中新增事件类型或数据结构？
2. 是否需要在 `decibel/` 中新增 REST/WS/链上交互类型？
3. 策略变更是否影响 `StrategyState` 或状态转移图？
4. 配置变更是否在 `config.Config` 中定义、在 `Defaults()` 中设默认值、在示例 `config.yaml` 中说明？
5. 是否补充了单元测试？

---

## 10. 已知限制与 TODO

代码库中存在以下待完成项（来自 README Roadmap 和代码注释）：

| 状态 | 项目 | 影响 |
|------|------|------|
| ❌ | Full WebSocket message parsing and event mapping | WS 部分消息类型可能解析不完整 |
| ❌ | Integration tests on testnet | 缺乏端到端验证 |
| ❌ | Risk guard integration into main loop | `risk_guard.go` 已存在但尚未接入 `main.go` 的事件循环 |
| ❌ | Prometheus metrics 实际更新 | `metrics.go` 已定义但策略中未调用 `Set`/`Inc` |
| ❌ | Vault integration | 机构资金托管方案 |
| ✅ | Bulk orders on-chain implementation | 已完成基础实现 |

---

## 11. AI 助手修改代码时的关键注意点

1. **状态机一致性**：任何修改 `DecibelMM` 状态转移逻辑的代码，必须对照第 5 节的状态转移图检查闭环。
2. **事件顺序敏感**：`HandleEvent` 中，`OrderUpdate` 和 `PositionUpdate` 的处理顺序会影响策略决策。确保 WS 事件先同步到 `OrderManager` / `PositionManager`，再进入策略。
3. **Bulk Order 与单订单互斥**：`UseBulkOrders=true` 时，策略使用 `submitBulkOrders` 批量替换某市场的所有订单；为 false 时走单笔下/撤单路径。不要在同一 tick 中混用两种模式。
4. **decimal 精度**：修改任何价格/数量计算时，检查是否需要 `quantize()` 截断到 `PriceDecimals`（9）或 `SizeDecimals`（9）。
5. **Aptos 地址比较**：永远使用 `decibel.AddrEqual()` 而非字符串直接比较。
6. **并发安全**：`EventBus.Publish` 是非阻塞的（满时丢弃），如果业务要求事件不丢失，不要增大 channel 缓冲到无限。
7. **TIF 选择**：
   - 做市挂单方：`TIFPostOnly`（`post_only: true`）
   - 市价平仓：`TIFImmediateOrCancel`
   - 普通限价：`TIFGoodTillCanceled`
