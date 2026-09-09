# Decibel Grid Bot — Rust TUI + Engine

Decibel DEX 网格策略规划、监控与 reconciliation-based 执行工具。支持 **Spot**（仅 PFS 余额）和 **Perp**（四种方向模式）在 testnet / mainnet 运行。

## 快速开始

```bash
cd grid-bot/rust
cp .env.example .env
# 填入 DECIBEL_API_KEY、SUBACCOUNT_ADDRESS、APTOS_PRIVATE_KEY
cargo run -- check-key --network testnet
cargo run -- doctor --product perp --market BTC/USD --subaccount 0x...
cargo run -- start --network testnet --product perp --market BTC/USD --subaccount 0x...
```

## Perp 方向模式

| 模式 | CLI `--perp-mode` | 行为 |
|---|:---:|---|
| 中性方向性 ladder | `neutral` | 双边对称网格，无方向偏好 |
| 做多方向性 ladder | `long` | 目标正仓位，ask 止盈 |
| 做空方向性 ladder | `short` | 目标负仓位，bid 止盈 |
| 旋转网格 | `rotate` | 每笔成交后自动补对侧止盈单，实现循环套利 |

### 旋转网格（Rotate）说明

`rotate` 模式追踪每个被动成交的 inventory lot：

```
Bid @ 78,000 filled
→ 创建 InventoryLot { entry_side: Bid, entry_price: 78,000, exit_price: 78,400 }
→ 下一周期自动在 78,400 补 Ask 止盈单

Ask @ 78,450 filled (命中 exit)
→ 关闭对应的 InventoryLot，标记为 Closed
→ 该 lot 的 exit order 从 ladder 中移除
```

- 初始 ladder 来自配置的固定价格几何区间
- entry 订单成交后，对应 exit 价格覆盖该价位（不会同时挂 entry 和 exit 在同一价格）
- exit 成交后恢复 entry capacity
- 重新启动会从 journal 恢复所有 open lots

## 引擎执行流程

每个刷新周期：

1. 并行获取：市场信息、mid 价格、账户概览、仓位、open orders、成交历史
2. WS hydration gate：等待 WebSocket 深度和 mid 数据就绪
3. 构建网格计划（pinned geometry，只随 fill 重建）
4. 风险检查：worst-case 暴露、margin gate、position max、pre-submit 仓位重读
5. 对账：对比期望 ladder 与实际 resting orders
6. 有缺档且全部门禁通过 → 原子替换整条 bulk ladder
7. 记录 journal event、更新本地 socket 状态、等待下一周期

### 交易前竞态保护

风险检查后、交易签名前，引擎会：

1. 重新读取链上最新仓位
2. 若 position 比检查时变化超过 lot_size，用新仓位重新运行风控
3. 若新仓位触发风控拒绝，取消本轮提交
4. 窗口从 risk check 到签名的 ~10s 缩小为 refresh 到签名的 ~100ms

## CLI 命令概览

| 命令 | 用途 | 是否交易 |
|---|---|---|
| `start` | 启动常驻引擎（返回后引擎继续运行） | 是 |
| `stop` | 停止引擎：先撤单，再按退出模式保留/清仓 | 是（撤单） |
| `attach` | 实时 TUI 面板（ratatui 全屏） | 否 |
| `status` | 单次快照 | 否 |
| `logs` | tail 引擎日志 | 否 |
| `doctor` | 读取市场、计划、余额、订单并报告风险 | 否 |
| `reconcile` | 期望网格与实际订单对比 | 否 |
| `shadow` | 连续模拟 reconciliation，不签名 | 否 |
| `check-key` | 校验 API key | 否 |
| `preview` | 离线预览网格与收益 | 否 |
| `tui` | 配置/预览/监控 TUI（不交易） | 否 |
| `simulate` | 离线多步场景 | 否 |
| `journal status` | 查看运行 journal 状态 | 否 |
| `journal resolve-divergence` | 手动恢复 diverged bulk ladder | 否 |

## 常驻引擎（生产使用）

```bash
# 启动
cargo run -- start --network testnet --product perp --market BTC/USD --subaccount 0x...

# 监控
cargo run -- attach --subaccount 0x...
cargo run -- logs -f --subaccount 0x...
cargo run -- status --subaccount 0x...

# 停止
cargo run -- stop --subaccount 0x...             # 撤单保留仓位
cargo run -- stop --subaccount 0x... --exit-mode liquidate  # 撤单并清仓
```

每个 subaccount 的控制文件在 `/tmp/grid-bot/<normalized-subaccount>.sock`。

`start` 持有 startup 锁防止双 spawn，引擎持有 run 锁。同一 subaccount 不能有两个进程同时提交 bulk ladder。

引擎收到 SIGINT/SIGTERM 时执行优雅停止（先撤单，再按退出模式处理）。`stop` 或引擎退出时写 `Shutdown` journal event。

## Pre-flight 诊断

引擎启动时运行：

1. WS 连接建立（15s 超时）
2. 深度健康检查（bid/ask spread 正常）
3. Mid price sanity（非零且波动在阈内）
4. 全部通过才进入主循环

失败时报错退出，不会卡在无限循环。

## Attach TUI

`attach` 是独立的实时 ratatui 面板，通过 Unix socket 订阅引擎状态：

- 连接后收到完整快照，随后只接收增量更新
- socket 断开后自动重连（1/2/4/8/16/30s 退避）
- 宽屏（≥113 列）且引擎有日志路径时，自动分两列：左 ladder，右日志侧栏
- 日志颜色：绿=成功、黄=警告、红加粗=错误、灰=信息
- 滚屏：`↑/↓`、`PgUp/PgDn`、`Home/End`、`[/]`（日志侧栏）、`f`（日志跟随）
- `c` 复制完整状态到剪贴板、`s` 打开清仓确认框、`q` 退出

头部显示 WS engine phase：`Ready` / `Connecting` / `Hydrating` / `Desynced` / `Blocked`。

## 配置优先级

```
CLI 参数 > 环境变量 / .env > 已保存档案 > 内置默认值
```

所有配置保存在 `~/.config/decibel-grid/profiles.json`（API Key 用 Argon2id + XChaCha20-Poly1305 加密）。

### `.env` 最小配置

```dotenv
DECIBEL_API_KEY=your_key
NETWORK=testnet
PRODUCT=perp
MARKET=BTC/USD
SUBACCOUNT_ADDRESS=0x...
APTOS_PRIVATE_KEY=0x...

# 方向模式：neutral / long / short / rotate
PERP_GRID_MODE=neutral

# 网格参数
GRID_TOTAL_COUNT=40
GRID_TOTAL_BUDGET=1000
GRID_RANGE_PERCENT=10
```

### Perp 参数速查

| 参数 | CLI flag | 默认 | 说明 |
|---|---|---|---|
| 方向模式 | `--perp-mode` | `neutral` | `neutral` / `long` / `short` / `rotate` |
| 档位数 | `--grid-count` | 40 | 双边合计（max 40） |
| 预算 | `--total-budget` | — | 推导每格数量的保证金预算 |
| 区间下界 | `--lower-price` | — | 固定区间底部价格 |
| 区间上界 | `--upper-price` | — | 固定区间顶部价格 |
| 区间百分比 | `--range-percent` | — | 中间价 ±N% |
| 每格百分比 | `--grid-step-percent` | — | 复合间距约 N% |
| 每格固定大小 | `--order-size` | — | 替代 budget |
| 最大仓位 | `--max-position` | — | 绝对 base 上限 |
| 刷新秒数 | `--refresh-seconds` | 5 | 周期轮询间隔 |
| 价格源 | `--price-source` | `prices` | `prices` / `depth` |
| 出界动作 | `--out-of-range` | `pause` | `pause` / `cancel_orders` / `close_position` / `clamp_continue` |
| 退出资产策略 | `--exit-asset-policy` | `retain` | `retain` / `sell` |
| 杠杆预览 | `--preview-leverage` | 1 | 保证金预览用 |
| 未成交费率 | `--maker-fee-rate` | 0.0001 | 预览用 |
| dry-run | `--dry-run` | false | 不发送交易，只模拟 |

### 可选：通知

```dotenv
DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/...
TELEGRAM_BOT_TOKEN=123456:token
TELEGRAM_CHAT_ID=123456789
TELEGRAM_ALLOWED_USER_IDS=123456789
```

### 可选：Gas Station 代付

```dotenv
GEOMI_GAS_STATION_API_KEY=your_key
```

未设置时签名账户自付 APT gas。

## Journal 与恢复

每个运行写入 append-only event journal（JSON Lines），存储在：

```
~/.local/share/decibel-grid/runs/<run_id>/
events.jsonl
state.json
```

journal 不保存原始子账户地址，仅保存不可逆 SHA3-256 指纹。

### CLI 恢复命令

```bash
journal status                # 查看当前运行状态
journal resolve-divergence    # 手动恢复 diverged bulk ladder（需 --confirm-operation）
perp bootstrap-clear-blocked  # 清除 blocked bootstrap 状态
perp bootstrap-inspect        # 查看 bootstrap 详情
```

## 安全设计

- Spot bulk 执行只使用 PFS 余额；Cross/CBS 的 quote 不能直接用于 bulk order
- 没有 client-order-id，不自动认定价格/数量相同的订单是自家订单
- 启动先进入 reconcile-only；未管理订单拒绝自动取消
- 主网执行需要 `--confirm-mainnet MAINNET` 显式确认
- bulk cancel 失败时区分 `ERESOURCE_DOES_NOT_EXIST`（视为成功）和真正 VM 失败

## 构建与测试

```bash
cargo build --release
cargo test                 # 165+ tests, 零网络
cargo fmt --check
cargo clippy --all-targets -- -D warnings

# 离线多步场景
cargo run -- simulate --scenario tests/scenarios/spot_pin_sweep.json
```

## 研究参考

项目受 Hummingbot V2、Passivbot、Freqtrade、OctoBot 设计启发。

### 对本项目的关键影响

- Exchange 订单是事实来源；本地 journal 是审计和 intent 归属来源
- 没有 client-order-id 时，不自动认定与期望价格/数量相同的订单是自家订单
- 启动先进入 reconcile-only；未管理订单拒绝自动取消
- 纯函数 planning/risk core 被 preview、reconcile、execute 共享

## 项目布局

```
src/
├── engine/mod.rs      主循环：周期、风控、journal、bulk 提交
├── strategy/          GridStrategy trait + 策略实现
│   ├── spot/          Spot 网格规划、runtime
│   └── perp/          Perp 网格规划、风险、方向模式
│       ├── rotate.rs  旋转网格 (RotatingGridState, InventoryLot)
│       ├── planning.rs uniform range 价格生成
│       ├── risk.rs    worst-case 暴露、margin、position trim
│       ├── runtime.rs  收敛、提交阻塞、平坦退出
│       └── ...
├── journal.rs         可持久化 append-only event log
├── control.rs         EnginePhase, EngineStatus, 控制面协议
├── ws_state.rs        WebSocket 状态：depth、fills、bulk ladder
├── attach_tui.rs      ratatui 实时监控面板
├── cli/               CLI 参数解析、子命令
├── client.rs          Decibel REST API 客户端
├── reconcile.rs       期望 vs 实际订单对比
├── spot_lifecycle.rs  Spot 订单生命周期（cancel、funding）
└── ...
```

## 多语言

界面支持英文（默认）和中文。在 Configure Tab 按 `Space` 切换。