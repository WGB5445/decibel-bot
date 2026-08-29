# Decibel Python 网格机器人

一个基于 Python 3.11+ 和 [`decibel-python-sdk`](https://pypi.org/project/decibel-python-sdk/) 的 Decibel 网格交易机器人，同时支持：

- **Spot 现货网格**：调用 `place_spot_bulk_order`。
- **Perpetual 合约网格**：调用 `place_bulk_orders`。
- **只读预览**：预览盘口、网格档位、手续费、收益情景和账户资金，不发送交易。
- **Dry-run 模式**：执行完整刷新循环，但不发送链上交易。
- **CLI 参数覆盖**：网格上下限、档位数量、每档数量、市场、刷新间隔等都可以直接通过命令行传入。
- **资金预检查**：Spot 检查 base/quote 余额；Perp 检查可用保证金，并在不足时显示明确警告。
- **40 档/边限制**：按照 Etna Move 源码，单个 bulk order 最多 40 个 Bid 和 40 个 Ask。

> 这是交易执行工具，不是投资建议，也不保证盈利。正式运行前必须先在 testnet 使用 `--preview` 和 `--dry-run` 验证。

---

## 目录

- [工作原理](#工作原理)
- [安全限制和风险](#安全限制和风险)
- [安装](#安装)
- [配置方式](#配置方式)
- [环境变量](#环境变量)
- [CLI 参数](#cli-参数)
- [预览网格和资金](#预览网格和资金)
- [收益和手续费计算](#收益和手续费计算)
- [Spot 现货网格](#spot-现货网格)
- [Perp 合约网格模式](#perp-合约网格模式)
- [Dry-run 和实盘运行](#dry-run-和实盘运行)
- [40 档限制](#40-档限制)
- [资金要求](#资金要求)
- [sequence 和并发运行](#sequence-和并发运行)
- [测试和代码检查](#测试和代码检查)
- [故障排查](#故障排查)

---

## 工作原理

机器人每隔 `GRID_REFRESH_SECONDS` 执行一次刷新：

1. 从 Decibel order book 读取最优 Bid 和 Ask。
2. 计算当前中间价：

   ```text
   mid = (best_bid + best_ask) / 2
   ```

3. 根据 `GRID_LOWER_PRICE`、`GRID_UPPER_PRICE` 和 `GRID_LEVELS_PER_SIDE` 生成价格档位。
4. 按市场的 `tick_size` 对价格进行对齐，按 `lot_size` 对下单数量进行对齐。
5. 检查 `min_size`，低于交易所最小数量时拒绝生成网格。
6. Perp 模式下检查当前仓位和最坏情况下的潜在仓位。
7. 发送新的 bulk order sequence。
8. 链上成功接受新的 bulk order 后，新的网格替代此前同一账户、同一产品、同一市场的 bulk grid。
9. 程序退出时尝试取消当前网格。

机器人使用的是 **POST_ONLY / maker 网格思路**。具体的链上订单行为、撮合、手续费和风险参数以 Decibel 当前部署的合约为准。

---

## 安全限制和风险

### 必须理解的风险

网格策略不是无风险套利：

- 单边下跌可能不断成交 Bid，积累多仓或 Spot 库存。
- 单边上涨可能不断成交 Ask，积累空仓或卖出 Spot 库存。
- Perp 可能产生 funding、强平和穿仓风险。
- 盘口移动会导致订单成交后无法在预期价格完成反向成交。
- API、RPC、网络、进程重启或 sequence 不同步可能导致网格没有及时更新。
- 预览收益只是理论情景，不代表真实成交收益。

### 程序的保护行为

- `--preview`：只读，不发送下单和取消订单交易。
- `--dry-run`：运行刷新循环，但不发送链上交易。
- Spot 预览会检查所有 Bid 所需 quote 资金和所有 Ask 所需 base 资金。
- Perp 预览会检查保守估算的保证金需求。
- `GRID_MAX_POSITION` 会限制 Perp 潜在仓位；超过限制时取消网格，不会自动平仓。
- 任何刷新异常时，Perp 实盘模式会尝试取消当前网格，避免在状态不明确时继续挂单。
- 同一个 subaccount、产品和市场不能同时运行多个机器人进程。

---

## 安装

推荐使用 [uv](https://docs.astral.sh/uv/)，不需要手动创建和激活虚拟环境：

```bash
cd /Users/logan/code/github/wgb5445/decibel-bot/grid-bot/python

uv sync --dev
cp .env.example .env
```

`uv sync --dev` 会自动创建 `.venv` 并安装 `decibel-python-sdk`、`python-dotenv` 以及开发依赖。

之后所有命令都用 `uv run` 前缀执行，不需要 `source .venv/bin/activate`：

```bash
uv run python grid_bot.py --help
uv run pytest
uv run ruff check .
```

**本文档后面的示例为了简洁写成 `python grid_bot.py ...`，使用 uv 时请统一改成 `uv run python grid_bot.py ...`。**

如果不想用 uv，也可以使用传统 venv：

```bash
python3 -m venv .venv
source .venv/bin/activate
pip install -e '.[dev]'
```

> 注意：`ModuleNotFoundError: No module named 'decibel'` 通常说明你用的是系统 `python3`，而不是本项目环境里的解释器。用 `uv run python grid_bot.py ...`，或者先激活 `.venv` 再运行。

程序启动时会自动读取当前目录下的 `.env`。CLI 参数和已导出的 shell 环境变量优先级都高于 `.env`。

不要把包含私钥的 `.env` 提交到 Git。

---

## 配置方式

支持两种配置来源：

1. `.env` / shell 环境变量；
2. CLI 参数。

优先级为：

```text
CLI 参数 > shell 环境变量 > .env > 程序默认值
```

例如：

```bash
# .env 中是 20 档
GRID_LEVELS_PER_SIDE=20

# CLI 指定 40 档，CLI 生效
python grid_bot.py spot --preview --levels-per-side 40
```

### 推荐启动流程

```bash
# 1. 只读预览
python grid_bot.py spot --preview

# 2. dry-run，验证刷新循环
python grid_bot.py spot --dry-run

# 3. testnet 小资金实盘验证
DRY_RUN=false python grid_bot.py spot

# 4. 确认日志、资金和成交行为后，再考虑 mainnet
```

---

## 环境变量

### 身份和网络

| 变量 | 是否必需 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `PRIVATE_KEY` | 实盘必需 | 无 | Aptos Ed25519 私钥，十六进制，允许带 `0x` 前缀。仅预览时可以不用。 |
| `DECIBEL_API_KEY` | **必需** | 无 | Decibel Dashboard API key。用于 Decibel REST / WebSocket 的行情、市场、费率与账户查询。 |
| `SUBACCOUNT_ADDRESS` | 可选 | 从私钥推导 | Decibel subaccount 地址。预览时如果没有私钥，需要设置它才能查询账户手续费和余额。 |
| `NETWORK` | 可选 | `testnet` | SDK 网络预设，例如 `testnet`、`mainnet`。 |
| `APTOS_NODE_API_KEY` | 建议配置 | 无 | Aptos fullnode RPC API key，实盘签名、gas 估算和提交交易时使用。也支持 `NODE_API_KEY` 作为兼容回退。 |

### 市场和网格

| 变量 | 是否必需 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `MARKET` | 可选 | `BTC/USD` | 市场名称，必须和 `spot` 或 `perp` 产品匹配。 |
| `GRID_LOWER_PRICE` / `GRID_UPPER_PRICE` | 三选一 | 无 | 固定上下边界，必须成对设置，且实时中间价必须落在区间内。 |
| `GRID_RANGE_PERCENT` | 三选一 | 无 | 围绕实时中间价的对称范围；`10` 表示中间价 ±10%。 |
| `GRID_STEP_PERCENT` | 三选一 | 无 | 相邻网格档位的复合百分比间距；`0.5` 表示每档约 0.5%。 |
| `GRID_TOTAL_COUNT` | 推荐 | 无 | Bid + Ask 的总订单数，范围 2–80；例如 `40` 通常是 20 Bid + 20 Ask。 |
| `GRID_LEVELS_PER_SIDE` | 兼容旧配置 | `40` | 单边档位数 1–40；未设置 `GRID_TOTAL_COUNT` 时使用。 |
| `GRID_TOTAL_BUDGET` | 推荐 | 无 | 总 quote/USDC 预算，自动推导每档数量。 |
| `GRID_ORDER_SIZE` | 兼容旧配置 | 无 | 固定的每档 base 数量；不能与 `GRID_TOTAL_BUDGET` 同时设置。 |
| `GRID_REFRESH_SECONDS` | 可选 | `20` | 网格刷新间隔。 |

### 合约和预览

| 变量 | 是否必需 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `PERP_GRID_MODE` | Perp 可选 | `neutral` | `neutral`、`long` 或 `short`。 |
| `GRID_MAX_POSITION` | Perp 可选 | 无 | 最大绝对仓位，使用 base asset 数量。 |
| `GRID_MAKER_FEE_RATE` | 可选 | 自动读取 | Maker 手续费率。`0.0001` 表示 0.01%，不是 0.0001 bps。 |
| `PREVIEW_LEVERAGE` | 可选 | `1` | 只用于预览保证金估算，不会修改链上杠杆。 |
| `DRY_RUN` | 可选 | `false` | `true` 时刷新循环不发送交易。 |
| `LOG_LEVEL` | 可选 | `INFO` | 日志等级。 |

一个完整的 `.env` 示例：

```dotenv
PRIVATE_KEY=0xYOUR_PRIVATE_KEY
DECIBEL_API_KEY=your_decibel_dashboard_api_key
APTOS_NODE_API_KEY=your_aptos_fullnode_rpc_key
SUBACCOUNT_ADDRESS=

NETWORK=testnet
MARKET=BTC/USD

# 推荐模式：实时中间价上下各 10%，合计 40 个订单，使用 1,000 USDC 预算。
GRID_RANGE_PERCENT=10
GRID_TOTAL_COUNT=40
GRID_TOTAL_BUDGET=1000
GRID_REFRESH_SECONDS=20

PERP_GRID_MODE=neutral
GRID_MAX_POSITION=0.05
GRID_MAKER_FEE_RATE=0.0001
PREVIEW_LEVERAGE=1

DRY_RUN=true
LOG_LEVEL=INFO
```

---

## CLI 参数

查看完整帮助：

```bash
python grid_bot.py --help
```

### 产品参数

必须指定一个产品：

```bash
python grid_bot.py spot ...
python grid_bot.py perp ...
```

### 网格参数

| 参数 | 对应环境变量 | 说明 |
| --- | --- | --- |
| `--market` | `MARKET` | 市场名称。 |
| `--lower-price` / `--upper-price` | `GRID_LOWER_PRICE` / `GRID_UPPER_PRICE` | 固定上下边界；两个必须一起给出。 |
| `--range-percent` | `GRID_RANGE_PERCENT` | 围绕实时中间价的对称范围，例如 `10` 即 ±10%。 |
| `--grid-step-percent` | `GRID_STEP_PERCENT` | 每档复合百分比间距，例如 `0.5` 即每档约 0.5%。 |
| `--grid-count` | `GRID_TOTAL_COUNT` | 推荐：Bid + Ask 合计订单数，范围 2–80。 |
| `--levels-per-side` | `GRID_LEVELS_PER_SIDE` | 兼容旧模式的单边档位数，最大 40。 |
| `--total-budget` | `GRID_TOTAL_BUDGET` | 推荐：总 quote/USDC 预算，自动计算每档数量。 |
| `--order-size` | `GRID_ORDER_SIZE` | 兼容旧模式的固定每档 base 数量。 |
| `--refresh-seconds` | `GRID_REFRESH_SECONDS` | 刷新间隔。 |
| `--max-position` | `GRID_MAX_POSITION` | Perp 最大仓位。 |

### 账户和预览参数

| 参数 | 对应环境变量 | 说明 |
| --- | --- | --- |
| `--network` | `NETWORK` | 网络。 |
| `--subaccount` | `SUBACCOUNT_ADDRESS` | 指定 subaccount。 |
| `--decibel-api-key` | `DECIBEL_API_KEY` | Decibel REST / WebSocket API key；必需。 |
| `--aptos-node-api-key` | `APTOS_NODE_API_KEY` | Aptos fullnode RPC API key，用于交易提交和 gas 查询。 |
| `--maker-fee-rate` | `GRID_MAKER_FEE_RATE` | 覆盖 maker 手续费率。 |
| `--preview-leverage` | `PREVIEW_LEVERAGE` | 预览保证金估算杠杆。 |
| `--preview` | 无 | 单次只读预览；不能与 `--log-file` 同时使用。 |
| `--dry-run` | `DRY_RUN` | 持续刷新但不发送交易。 |
| `--log-file` | `LOG_FILE` | 覆盖写入日志文件，并将 stdout/stderr 重定向到该文件。 |
| `--spot-funding-amount` | `SPOT_FUNDING_AMOUNT` | Spot 启动前从 Cross 转入 PFS 的 USDC 数量；只支持 Cross → PFS。 |
| `--spot-funding-metadata` | `SPOT_FUNDING_METADATA` | Cross → PFS 使用的 quote asset metadata 地址。 |

`--log-file PATH`（或 `LOG_FILE`）会覆盖写入日志文件，并将 Python 的 stdout/stderr 和 logging 输出都写入其中；不能与 `--preview` 同时使用。

### 合约模式参数

```bash
--perp-mode neutral
--perp-mode long
--perp-mode short
```

---

## 总预算、总网格数和价格范围

推荐不再手工猜 `GRID_ORDER_SIZE`，而是输入 **总预算** 和 **总订单数**。机器人会读取实时中间价、市场 tick/lot/min-size 和 maker fee，然后计算最终每档价格、每档数量、资金占用与费用后的情景收益。

### 推荐命令

下面的例子表示：总预算 `1000 USDC`、总共 `40` 个订单、围绕实时中间价上下 `10%` 的 Neutral Perp 网格。最终通常为 `20 Bid + 20 Ask`：

```bash
uv run python grid_bot.py perp --preview \
  --network mainnet \
  --market BTC/USD \
  --range-percent 10 \
  --grid-count 40 \
  --total-budget 1000 \
  --perp-mode neutral \
  --preview-leverage 1 \
  --price-source prices
```

`--preview` 会打印最终每一档：

```text
BID  1: price × size = notional
...
ASK 20: price × size = notional
```

因此你可以在任何交易前检查实际 tick/lot 对齐后的订单，而不是只看理论参数。

### 总订单数的含义

`--grid-count` / `GRID_TOTAL_COUNT` 表示 **Bid 和 Ask 合计的实际订单数量**，范围是 `2–80`：

| `--grid-count` | Neutral 网格分布 |
| ---: | --- |
| `10` | 5 Bid + 5 Ask |
| `40` | 20 Bid + 20 Ask |
| `41` | 20 Bid + 21 Ask |
| `80` | 40 Bid + 40 Ask（链上单次 bulk 上限） |

Long Perp 使用全部订单作为 Bid；Short Perp 使用全部订单作为 Ask。tick rounding 可能合并过于接近的价格，因此最终订单数可能少于请求数量。

### 三种价格范围模式：三选一

不要同时设置多个范围模式；CLI 范围参数会整体覆盖 `.env` 中的范围参数。

#### 1. 固定上下界

适合你已经有明确的价格区间：

```bash
--lower-price 65000 --upper-price 80000
```

实时中间价必须位于区间内，否则机器人拒绝构造单边伪网格。

#### 2. 中间价 ± 百分比

适合“当前价格上下 X%”的策略：

```bash
--range-percent 10
```

如果中间价为 `71547.4`，实际区间约为：

```text
lower = 71547.4 × (1 - 10%) = 644... 
upper = 71547.4 × (1 + 10%) = 787...
```

这可以避免把旧行情的固定区间（例如 `90000–110000`）错误用于当前 `71547.4` 的市场。

#### 3. 每档百分比间距

适合希望每一格有固定百分比价差的策略：

```bash
--grid-step-percent 0.5 --grid-count 40
```

Bid 第 `i` 档使用：

```text
mid × (1 - 0.5%)^i
```

Ask 第 `i` 档使用：

```text
mid × (1 + 0.5%)^i
```

这是复合间距，不是线性美元间距。该模式会根据总订单数自动推导最远上下界。

### 总预算的分配规则

总预算参数：

```bash
--total-budget 1000
```

或：

```dotenv
GRID_TOTAL_BUDGET=1000
```

- **Spot**：预算约 50% 分配给所有 Bid 的 quote 预留（包含 maker fee 缓冲），另 50% 以所有 Ask 的挂单价值估算需要持有的 base inventory。每侧使用统一、lot 对齐的数量。预览仍会检查你真实的 base 和 quote 余额。
- **Neutral Perp**：将预算视为保守保证金预算。每档统一数量按较大一侧名义价值、`PREVIEW_LEVERAGE` 和 maker-fee 缓冲反推。
- **Long / Short Perp**：预算只分配给启用的一侧。

由于数量必须按 lot size 向下对齐，实际资金使用通常会略低于输入预算；如果预算不足以得到一个 `min_size` 的订单，预览会明确拒绝。

`--total-budget` 和 `--order-size` **不能同时使用**。前者推荐用于普通使用；后者仅适合你明确知道每档 base 数量时。

---

## 预览网格和资金

`--preview` 是正式运行前最重要的命令。它会:

1. 读取实时盘口。
2. 读取市场精度：`tick_size`、`lot_size`、`min_size`。
3. 构造最终 Bid / Ask 数量。
4. 读取或使用指定的 maker fee。
5. 估算理论网格捕获收益。
6. 计算资金需求。
7. 查询账户余额或可用保证金。
8. 在资金不足时输出 `WARNING`。

### Spot 预览示例

```bash
python grid_bot.py spot --preview \
  --network testnet \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 40 \
  --order-size 0.001
```

典型输出字段：

```text
Product / market: spot / BTC/USD
Bulk levels: 40 bids + 40 asks (limit: 40/side)
Maker fee: 0.01000000%
Required quote for bids: ...
Available quote: ...
Required base for asks: ...
Available base: ...
Funding check: sufficient for the previewed worst-case reservation.
```

如果 quote 不足：

```text
WARNING: INSUFFICIENT QUOTE: not enough quote asset to reserve all spot bids.
```

如果 base 不足：

```text
WARNING: INSUFFICIENT BASE: not enough base asset to reserve all spot asks.
```

### Perp 预览示例

```bash
python grid_bot.py perp --preview \
  --network testnet \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 40 \
  --order-size 0.001 \
  --perp-mode neutral \
  --max-position 0.05 \
  --preview-leverage 5
```

典型输出字段：

```text
Product / market: perp / BTC/USD
Perp mode: neutral
Bulk levels: 40 bids + 40 asks (limit: 40/side)
Estimated required perp margin: ... USDC
Available perp margin: ... USDC
```

如果保证金不足：

```text
WARNING: INSUFFICIENT MARGIN: available margin is below the conservative grid requirement.
```

### API Key 配置（主网必须确认）

机器人使用两类不同用途的 key，**不要混用**：

```dotenv
# Decibel Dashboard 提供；用于 Decibel REST / WebSocket 查询，程序必需。
DECIBEL_API_KEY=your_decibel_dashboard_api_key

# Aptos RPC 提供商或节点提供；用于交易签名后的 fullnode 请求与 gas 查询，实盘建议配置。
APTOS_NODE_API_KEY=your_aptos_fullnode_rpc_key
```

也可以仅在当前命令中传入，避免写入 shell history 以外的文件：

```bash
uv run python grid_bot.py perp --preview --network mainnet \
  --decibel-api-key "$DECIBEL_API_KEY" \
  --aptos-node-api-key "$APTOS_NODE_API_KEY" \
  --lower-price 90000 --upper-price 110000 \
  --levels-per-side 10 --order-size 0.001
```

`DECIBEL_API_KEY` 缺失时，程序会在启动前报错；这避免了错误地用没有鉴权的请求访问主网 API。`APTOS_NODE_API_KEY` 当前可以留空，但取决于你使用的 Aptos fullnode 端点，实盘请求可能被限流或拒绝，因此主网实盘应配置它。

### 无私钥预览

如果设置了 `PRIVATE_KEY`，程序可以推导 subaccount 并读取账户数据：

```bash
python grid_bot.py perp --preview
```

如果不想在预览时提供私钥，可以使用账户地址和手续费率：

```bash
SUBACCOUNT_ADDRESS=0xYOUR_SUBACCOUNT \
python grid_bot.py spot --preview
```

或者：

```bash
GRID_MAKER_FEE_RATE=0.0001 \
python grid_bot.py perp --preview --perp-mode neutral
```

如果既没有 `PRIVATE_KEY`，也没有 `SUBACCOUNT_ADDRESS`，并且没有提供 `GRID_MAKER_FEE_RATE`，程序无法查询实际费率和账户余额，会直接提醒需要补充配置。无论哪种预览方式，仍然必须设置 `DECIBEL_API_KEY`。

---

## 收益和手续费计算

### 单个买卖配对

对于一个 Bid 和之后对应成交的 Ask：

```text
买入名义价值 = bid_price × size
卖出名义价值 = ask_price × size

毛收益 = 卖出名义价值 - 买入名义价值

买入手续费 = 买入名义价值 × maker_fee_rate
卖出手续费 = 卖出名义价值 × maker_fee_rate

双边手续费 = 买入手续费 + 卖出手续费

净收益 = 毛收益 - 双边手续费
```

合并为：

```text
净收益 = (ask_price - bid_price) × size
         - (bid_price × size + ask_price × size) × maker_fee_rate
```

### 示例

假设：

```text
Bid = 95
Ask = 105
Size = 1
Maker fee = 0.1% = 0.001
```

则：

```text
毛收益 = 105 - 95 = 10
买入手续费 = 95 × 0.001 = 0.095
卖出手续费 = 105 × 0.001 = 0.105
双边手续费 = 0.2
净收益 = 10 - 0.2 = 9.8
```

### 预览收益的限制

当前预览是一个“理论完成配对”的情景模型，假设：

- 每个 Bid 都能成交；
- 对应 Ask 之后也能成交；
- 成交数量完整；
- 使用当前 maker fee；
- 没有价格跳变和订单失效。

它没有计算：

- Funding fee；
- Gas 或链上交易成本；
- Taker fee；
- 滑点；
- 部分成交；
- 订单被取消或拒绝；
- 单边行情造成的浮亏；
- Perp 强平和风险限额；
- 资金占用成本；
- 手续费等级变化。

因此，`Estimated net grid capture` 不是预期日收益，也不是收益保证。

对于 `long` 和 `short` 单边合约网格，因为同一个 bulk order 内没有完整的买入—卖出闭环，预览会显示没有可配对的内部收益，而不会虚构收益数字。

---

## Spot 现货网格

Spot 网格同时挂两侧：

- 中间价下方挂 Bid；
- 中间价上方挂 Ask。

### Spot 所需资金

完整的 40/40 网格可能需要大量资金：

```text
Bid 资金需求 = 所有 Bid 的价格 × 数量之和 + 手续费缓冲
Ask 资金需求 = 所有 Ask 的 base 数量之和
```

如果账户没有足够 quote：

- 买单可能无法全部成功；
- 账户可能只能支撑部分网格；
- 下跌时成交会持续增加 base 库存。

如果账户没有足够 base：

- Ask 可能无法全部挂出；
- 即使挂出，也可能因为账户可用余额不足被链上拒绝。

### Spot 启动示例

```bash
# 先预览
python grid_bot.py spot --preview \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 20 \
  --order-size 0.001

# 再 dry-run
python grid_bot.py spot --dry-run \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 20 \
  --order-size 0.001

# 确认后才实盘
DRY_RUN=false python grid_bot.py spot \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 20 \
  --order-size 0.001
```

---

## Perp 合约网格模式

### `neutral`：中性双向网格

```bash
python grid_bot.py perp --preview --perp-mode neutral
```

行为：

- 下方挂 Bid；
- 上方挂 Ask；
- 初始仓位接近 0 时，理论上接近 delta-neutral；
- 但成交不对称会使账户产生多仓或空仓。

例子：

- 价格下跌，更多 Bid 成交，账户逐渐变成多仓；
- 价格上涨，更多 Ask 成交，账户逐渐变成空仓。

推荐搭配：

```bash
--max-position 0.05
```

程序会把当前仓位和当前网格所有潜在同向成交纳入检查。超过限制时会取消网格，不会自动平仓。

### `long`：做多/买入累积网格

```bash
python grid_bot.py perp --preview --perp-mode long
```

行为：

- 只挂 Bid；
- 价格下跌时分层买入；
- 成交后增加多仓；
- 不自动生成上方止盈 Ask。

这个模式适合你有独立止盈或回补逻辑的场景。它不是完整的自动网格闭环，不能只看当前挂单计算最终收益。

必须配置仓位限制：

```bash
python grid_bot.py perp \
  --perp-mode long \
  --max-position 0.05 \
  --dry-run
```

### `short`：做空/卖出累积网格

```bash
python grid_bot.py perp --preview --perp-mode short
```

行为：

- 只挂 Ask；
- 价格上涨时分层开空；
- 成交后增加空仓；
- 不自动生成下方回补 Bid。

必须有独立的止损、回补和平仓方案。

---

## Dry-run 和实盘运行

### 只读 Preview

只执行一次：

```bash
python grid_bot.py spot --preview
```

不会：

- 发送下单交易；
- 发送取消交易；
- 修改账户状态；
- 消耗链上交易 sequence。

### Dry-run 刷新循环

```bash
python grid_bot.py perp --dry-run --perp-mode neutral
```

会持续：

- 读取盘口；
- 计算网格；
- 检查仓位逻辑；
- 输出日志；
- 等待下一次刷新。

不会：

- 下单；
- 取消订单；
- 改变账户余额或仓位。

### 实盘运行

Spot：

```bash
DRY_RUN=false python grid_bot.py spot \
  --network testnet \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 10 \
  --order-size 0.001
```

Perp：

```bash
DRY_RUN=false python grid_bot.py perp \
  --network testnet \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 10 \
  --order-size 0.001 \
  --perp-mode neutral \
  --max-position 0.005
```

建议先使用较少档位和较小数量，不要一开始直接运行 40/40 最大网格。

停止程序：

```text
Ctrl-C
```

收到 `SIGINT` 或 `SIGTERM` 后，实盘模式会尝试取消当前 bulk grid。

---

## 40 档限制

Etna 源码中的限制是：

```move
const MAX_BULK_ORDER_DEPTH_PER_SIDE: u64 = 40;
```

并分别校验：

```move
num_bids <= 40
num_asks <= 40
```

因此：

```text
单边最多：40
双边最多：40 Bid + 40 Ask = 80 个价格档位
```

这里的 `GRID_LEVELS_PER_SIDE` 是单边数量，不是双边总数量：

```bash
GRID_LEVELS_PER_SIDE=40
```

表示最多：

```text
40 个 Bid + 40 个 Ask
```

但最终数量可能少于 40，原因包括：

- tick size 太大导致价格重复；
- 价格范围太窄；
- 当前 midpoint 不在区间中心；
- Long 模式只保留 Bid；
- Short 模式只保留 Ask。

---

## 资金要求

### Spot 资金

Spot bulk 订单只从 **PFS (Primary Fungible Store)** 取资金，不会直接使用 Cross/collateral 中的 USDC。现货账户中的 `positions` 是扣除 in-flight reservation 后的余额；在替换已有 bulk ladder 时，已有 escrow 会计入 replacement 可用量。

程序现在会：

- 区分 PFS base/quote、bulk escrow 和 Cross USDC；
- Spot 强制使用 order book mid，不使用 Perp 专用 `/prices` feed；
- 在首次铺网格前最多执行 6 次有界 IOC 买入，补足缺失 base；IOC 只使用 bid reserve 之外的 PFS quote；
- funding 完成后重新读取余额；如果仍不足，则保留最靠近 mid 的档位并按 `Bid 降序 / Ask 升序` 提交；
- 已有 bulk ladder 与目标完全一致时跳过 replacement；同样档位数量下的小幅漂移在 30 秒冷却期内跳过。

首次运行 Spot 时，推荐先预览：

```bash
python grid_bot.py spot --preview
```

如果 USDC 在 Cross 而不是 PFS，可显式请求 Cross → PFS 转账（负数方向由程序固定处理）：

```bash
python grid_bot.py spot \
  --spot-funding-amount 945.910599 \
  --spot-funding-metadata 0x5428acf5c112826d0c74ea1cd2de9030f53d1d01235e6c2621d967bf914ee1c8
```

程序只提交 `transfer_assets_between_non_collateral_and_collateral`，不会提交 `HOLD_AS_NON_COLLATERAL`：后者是 subaccount owner-only 设置，必须由 owner 在 Decibel UI/wallet 手动完成。Testnet USDC metadata 如上；mainnet 必须提供对应网络的 metadata 地址。

如果 PFS 资金不足，程序不会借贷；它会尝试上述一次性 IOC 补 base，并将最终 bulk ladder 缩减到实际 PFS/escrow 可支持的档位。你也可以通过以下方式降低资金需求:

- 减少 `--levels-per-side`；
- 减少 `--order-size`；
- 缩窄价格区间；
- 只运行一侧策略；
- 使用更合适的市场。

### Perp 保证金

Perp 预览使用保守估算：

```text
估算保证金
= max(所有 Bid 名义价值, 所有 Ask 名义价值) / PREVIEW_LEVERAGE
  + maker fee buffer
```

然后与 SDK 返回的 `cross_available_to_trade` 对比。

这只是预检查，因为真实链上可用保证金还会受到：

- 已有仓位；
- 已有挂单占用；
- 维护保证金；
- 风险档位；
- funding；
- 市场波动；
- 链上账户状态；
- Decibel 当前风控参数。

影响。

`PREVIEW_LEVERAGE` 不是交易杠杆设置：

```bash
# 仅把预览估算改成 5 倍杠杆
python grid_bot.py perp --preview --preview-leverage 5
```

不会修改账户的真实 leverage。

---

## sequence 和并发运行

Decibel bulk order 使用递增的 sequence number。

程序启动时会读取已有 bulk order,尝试从最大 sequence 后继续递增。刷新时提交新的 sequence。

程序会在启动时为 `(network, subaccount)` 获取一把非阻塞的文件锁（与市场/产品无关），锁文件位于：

```text
${DECIBEL_GRID_DATA_DIR:-~/.local/share}/decibel-grid/locks/subaccount-<sha256>.lock
```

同一 subaccount 的第二个进程会立即报错退出，而不是继续运行并与第一个进程竞争 bulk sequence 或 Spot funding。因此不要尝试在同一 subaccount 下并行运行多个实例，否则会看到：

```text
another grid process is already running for network <network> and this subaccount; stop it before starting a second instance
```

即使在锁生效之前，同时运行:

```text
同一个 subaccount + 同一个产品 + 同一个市场
```

的多个机器人实例仍然可能出现:

- sequence 冲突;
- 新订单被旧进程覆盖;
- 取消操作互相影响;
- 网格状态无法判断。

如果程序异常退出，重新启动前建议：

1. 先运行 `--preview`；
2. 检查账户当前 bulk order；
3. 确认没有其他机器人占用同一市场；
4. 再运行 `--dry-run`；
5. 最后才开启实盘。

---

## 故障排查

### `PRIVATE_KEY is required to run the bot`

实盘或 dry-run 刷新循环需要签名账户：

```bash
export PRIVATE_KEY=0x...
```

`--preview` 可以不需要私钥，但必须通过 `SUBACCOUNT_ADDRESS` 和/或 `--maker-fee-rate` 提供足够信息。

### `GRID_LOWER_PRICE must be less than GRID_UPPER_PRICE`

检查：

```bash
--lower-price 90000 --upper-price 110000
```

不能反过来。

### `mid price ... is outside the configured grid range`

当前实时 midpoint 不在网格范围内。扩大范围：

```bash
--lower-price 80000 --upper-price 120000
```

不要把区间设置得离当前价格太远或太窄。

### `GRID_ORDER_SIZE rounds to zero`

你的数量小于市场 lot size。增加：

```bash
--order-size 0.001
```

### `below the market minimum`

数量虽然符合 lot size，但低于市场 `min_size`。增加 `--order-size`。

### `INSUFFICIENT QUOTE`

Spot Bid 所需 quote 不足。减少档位或数量，或者补充 quote 资产。

### `INSUFFICIENT BASE`

Spot Ask 所需 base 不足。减少档位或数量，或者补充 base 资产。

### `INSUFFICIENT MARGIN`

Perp 预估保证金不足。降低数量/档位、提高可用保证金，或重新评估 `--preview-leverage`。不要仅仅为了让预览通过而盲目提高杠杆参数。

### `profit estimate is N/A`

当前是 `long` 或 `short` 单边模式。单边网格没有同时提交完整的买入—卖出循环，因此程序不生成虚假的闭环收益。

### 预览没有显示余额

检查：

```bash
SUBACCOUNT_ADDRESS=0x...
```

或者提供 `PRIVATE_KEY`。如果只设置了 `--maker-fee-rate`，程序可以计算手续费和理论收益，但不能确认账户真实余额。

---

## 测试和代码检查

```bash
cd grid-bot/python

uv run pytest
uv run ruff check .
uv run ruff format --check .
```

测试覆盖：

- 网格价格生成；
- Bid / Ask 排序；
- 40 档/边限制；
- Spot 和 Perp 模式；
- Long / Short 单边模式；
- tick / lot 精度缩放；
- 最小下单量；
- 双边 maker fee 扣除；
- Spot 资金需求估算；
- Perp 保证金估算。

测试不会向 Decibel 发送真实交易。

---

## 最小推荐操作清单

```bash
# 进入目录
cd /Users/logan/code/github/wgb5445/decibel-bot/grid-bot/python

# 预览 10 档/边，而不是一开始使用最大网格
uv run python grid_bot.py perp --preview \
  --network testnet \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 10 \
  --order-size 0.001 \
  --perp-mode neutral \
  --max-position 0.005 \
  --preview-leverage 1

# dry-run
uv run python grid_bot.py perp --dry-run \
  --network testnet \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 10 \
  --order-size 0.001 \
  --perp-mode neutral \
  --max-position 0.005

# 确认后再考虑实盘
DRY_RUN=false uv run python grid_bot.py perp \
  --network testnet \
  --market BTC/USD \
  --lower-price 90000 \
  --upper-price 110000 \
  --levels-per-side 10 \
  --order-size 0.001 \
  --perp-mode neutral \
  --max-position 0.005
```

如果使用 mainnet，请先确认 `NETWORK=mainnet`、私钥、市场、subaccount、资金和风险参数都正确。
