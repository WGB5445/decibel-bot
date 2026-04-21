# Decibel 做市机器人 — 配置文件完全指南

> 本文档专门讲解 `configs/config.yaml` 的每一项配置。
> 
> 如果你是完全不懂代码的小白，建议先阅读 `USAGE.md` 获取整体使用流程，本文档作为参数速查和深度参考。

---

## 快速开始：最小可用配置

把下面这段复制到 `configs/config.yaml`，然后**把你自己的信息填进去**就能跑：

```yaml
env: testnet

decibel:
  bearer_token: "你的API_TOKEN"
  api_wallet_private_key: "你的钱包私钥"
  api_wallet_address: ""
  subaccount_address: "你的子账户地址"
  market_name: "BTC-USD"

strategy:
  leverage: 2
  bid_spread: "0.005"
  ask_spread: "0.005"
  order_amount: "0.001"
  order_levels: 1
  order_refresh_time: "30s"
  filled_order_delay: "60s"
  stop_loss: "0.03"
  take_profit: "0.02"
  time_limit: "45m"
  use_bulk_orders: true
  post_only: true
```

---

## 配置文件的两种设置方式

机器人支持 **3 种方式**设置配置，优先级从高到低：

### 方式 1：命令行参数（最高优先级）

```bash
# Windows
.\decibel-bot.exe --config D:\我的配置\bot.yaml

# Mac/Linux
./decibel-bot --config ~/Documents/bot.yaml
```

### 方式 2：环境变量

```bash
# Windows PowerShell
$env:DECIBEL_CONFIG = "D:\我的配置\bot.yaml"
.\decibel-bot.exe

# Mac/Linux Terminal
export DECIBEL_CONFIG=/home/用户名/bot.yaml
./decibel-bot
```

### 方式 3：默认路径（最低优先级）

什么都不指定时，机器人自动寻找当前目录下的 `configs/config.yaml`。

---

## 配置项详解

### 一、环境设置（env）

```yaml
env: testnet
```

| 可选值 | 说明 | 建议 |
|--------|------|------|

| `testnet` | **测试网**（假钱，免费练习） | **新手先用这个！** |
| `mainnet` | **主网**（真钱，真交易） | 熟练后再切换 |

> 💡 **重要提示**：
> - `testnet` 上的钱是假的，你可以放心测试，即使亏光也不心疼
> - `mainnet` 上的每一笔交易都是真实的，会真的赚钱或亏钱
> - 建议先在 `testnet` 跑至少 3~7 天，确认参数合理后再切 `mainnet`

---

### 二、Decibel 账户设置（decibel）

这是**最重要也最容易出错**的部分，每一项都必须正确填写。

#### 2.1 bearer_token — API 通行证

```yaml
bearer_token: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."
```

**是什么？**
- 这是 Decibel 网站给你的"通行证"，证明你有权限访问 API

**怎么获取？**
1. 登录 [app.decibel.trade](https://app.decibel.trade)
2. 点击右上角头像 → "API 管理" 或 "API Keys"
3. 点击 "创建新 API Key"
4. 复制生成的长字符串

**注意事项：**
- 这串字符通常很长，以 `eyJ` 开头
- **千万不要泄露给任何人！** 别人拿到这个就能操作你的账户
- 如果怀疑泄露了，立刻去网站删除旧的，重新创建一个

---

#### 2.2 api_wallet_private_key — 钱包私钥

```yaml
api_wallet_private_key: "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
```

**是什么？**
- 这是你的 Aptos 钱包的"密码"
- 机器人需要用它来给交易签名（就像你刷卡要输密码一样）

**怎么获取？（以 Petra 钱包为例）**
1. 打开浏览器的 Petra 钱包插件
2. 点击钱包界面右上角的 "⚙️"（设置）
3. 找到 "导出私钥" 或 "显示私钥"
4. 输入钱包密码，复制显示的字符串

**常见的私钥格式：**

| 格式 | 示例 |
|------|------|
| 带 0x 前缀的 hex | `0x1234abcd...` （64位字符） |
| AIP-80 格式 | `ed25519-priv-0x1234abcd...` |

**注意事项：**
- ⚠️ **私钥 = 你的银行密码 + 银行卡，别人拿到就能转走你所有的钱**
- 不要截图存在手机里
- 不要通过微信/QQ/邮件发给别人
- 手抄在纸上，锁在抽屉里最安全

---

#### 2.3 api_wallet_address — 钱包地址（可选，推荐留空）

```yaml
api_wallet_address: ""
```

**是什么？**
- 这是你的 Aptos 钱包的"账号"，类似银行卡号

**需要填吗？**
- **不需要！** 机器人会自动从你的私钥推导出来
- 把这个字段留空 `""` 即可

**什么时候需要填？**
- 只有在一种情况下需要手动填写：**你想验证私钥和地址是否匹配**
- 如果你填了地址，但和私钥推导出来的不一致，机器人会发出警告

**注意事项：**
- 地址通常很长，以 `0x` 开头
- 私钥和地址不要搞混：**私钥是密码（要保密），地址是账号（可以给别人）**

---

#### 2.4 subaccount_address — 子账户地址

```yaml
subaccount_address: "0xdef789abc012def789abc012def789abc012def789abc012def789abc012def7"
```

**是什么？**
- 这是你在 Decibel 网站上创建的"交易子账户"
- 相当于在主钱包下面开了一个"专门给机器人用的分账户"

**为什么用子账户？**
- 把机器人的资金和手动交易的资金分开
- 即使机器人出问题，也不会影响主钱包里的其他钱
- 方便统计机器人的盈亏

**怎么获取？**
1. 登录 Decibel 网站
2. 进入交易页面
3. 找到 "子账户" 或 "Subaccount" 选项
4. 点击 "创建子账户"
5. 创建成功后，复制子账户地址

**注意事项：**
- 子账户地址和钱包地址格式一样，都是 `0x` 开头
- 不要把子账户地址和钱包地址填反了！
- 创建子账户后，记得往子账户里充值 USDC（见 USAGE.md）

---

#### 2.5 market_name — 交易市场

```yaml
market_name: "BTC-USD"
```

**是什么？**
- 你想让机器人为哪个币种做市

**可选值（常见）：**

| 市场名称 | 交易标的 | 波动程度 |
|----------|----------|----------|
| `BTC-USD` | 比特币 | 中等 |
| `ETH-USD` | 以太坊 | 中等偏大 |
| `SOL-USD` | 索拉纳 | 较大 |

**怎么确认某个市场是否存在？**

运行辅助工具查看所有可用市场：

```bash
# Windows
.\market-info.exe

# Mac/Linux
./market-info
```

输出示例：
```
名称              市场地址                                              精度(价格/数量)
--------------------------------------------------------------------------------
BTC-USD           0x1234abcd...                                        9 / 9
ETH-USD           0x5678efab...                                        9 / 9
```

**注意事项：**
- 名称必须和 Decibel 网站上显示的**完全一致**，包括大小写
- 常见错误：`btc-usd`（小写）或 `BTCUSDT`（没有横线）都是错的

---

#### 2.6 builder_code — 推荐人代码（可选）

```yaml
builder_code: ""
```

**是什么？**
- 如果你有推荐人，填他的推荐码，可以让推荐人获得部分手续费返佣
- 没有就留空 `""`

---

### 三、Aptos 节点设置（aptos）

```yaml
aptos:
  fullnode_url: ""
```

**是什么？**
- 这是机器人连接 Aptos 区块链的"服务器地址"
- 机器人通过它提交交易、查询交易状态

**需要改吗？**
- **一般不需要改！** 留空 `""` 即可
- 机器人会根据你设置的 `env` 自动选择对应的官方节点

**各环境默认节点：**

| env | 默认节点地址 | 说明 |
|-----|-------------|------|

| testnet | `https://fullnode.testnet.aptoslabs.com/v1` | Aptos 官方测试网节点 |
| mainnet | `https://fullnode.mainnet.aptoslabs.com/v1` | Aptos 官方主网节点 |

**什么时候需要自定义？**

如果你使用了第三方 RPC 服务商，或者自建了 Aptos 节点，可以在这里填写自定义地址：

```yaml
aptos:
  fullnode_url: "https://aptos-mainnet.nodereal.io/v1/你的API_KEY"
```

常见第三方 RPC 服务商：

| 服务商 | 网址 |
|--------|------|
| Alchemy | alchemy.com |
| QuickNode | quicknode.com |
| NodeReal | nodereal.io |
| Ankr | ankr.com |

> ⚠️ **注意**：自定义 RPC 必须和 `env` 对应的链一致。不要把主网 RPC 配到 testnet 环境里！

---

### 四、策略设置（strategy）

这是**最影响交易结果**的部分，每一个参数都直接决定机器人怎么买卖。

#### 4.1 leverage — 杠杆倍数

```yaml
leverage: 2
```

**是什么？**
- 杠杆就是"借钱做生意"
- 你有 100 USDC，设 2 倍杠杆，机器人会帮你做 200 USDC 的生意

**可选范围：**
- Decibel 支持的范围通常是 1~50 倍
- 但机器人代码里建议保守使用

**各杠杆档位对比：**

| 杠杆 | 你的 100 USDC 能做多大生意 | 涨 1% 赚多少 | 跌 1% 亏多少 |
|------|--------------------------|-------------|-------------|
| 1x | 100 USDC | 1 USDC | 1 USDC |
| 2x | 200 USDC | 2 USDC | 2 USDC |
| 5x | 500 USDC | 5 USDC | 5 USDC |
| 10x | 1000 USDC | 10 USDC | 10 USDC |

**建议：**
- **新手：1~2 倍**（几乎不会爆仓，先熟悉机器人行为）
- 有经验：3~5 倍
- 高手：5~10 倍（需配合严格的止损）

> ⚠️ **爆仓风险**：10 倍杠杆意味着价格反向波动 10% 就可能爆仓（全部亏光）！

---

#### 4.2 bid_spread — 买方价差

```yaml
bid_spread: "0.005"
```

**是什么？**
- 机器人挂"买单"（买入）时，比中间价低多少百分比
- `0.005` = 0.5%

**举个例子：**
- 比特币中间价 = 100,000 美元
- bid_spread = 0.005 (0.5%)
- 机器人的买单价格 = 100,000 × (1 - 0.5%) = **99,500 美元**

**设置建议：**

| 市场环境 | 建议值 | 说明 |
|----------|--------|------|
| 波动小、很平静 | 0.002 ~ 0.003 | 价差小，成交快，薄利多销 |
| 正常波动 | 0.005 ~ 0.008 | **推荐范围** |
| 波动大、不确定 | 0.01 ~ 0.02 | 价差大，留足安全边际 |

**注意事项：**
- 价差越大 → 单笔利润越高，但成交越难
- 价差越小 → 成交越容易，但单笔利润越少
- 要和 `ask_spread` 保持一致或接近

---

#### 4.3 ask_spread — 卖方价差

```yaml
ask_spread: "0.005"
```

**是什么？**
- 机器人挂"卖单"（卖出）时，比中间价高多少百分比
- `0.005` = 0.5%

**举个例子：**
- 比特币中间价 = 100,000 美元
- ask_spread = 0.005 (0.5%)
- 机器人的卖单价格 = 100,000 × (1 + 0.5%) = **100,500 美元**

**设置建议：**
- 通常和 `bid_spread` 设成一样的值
- 如果你认为上涨概率大，可以把 `ask_spread` 设大一点（少卖，留着涨）
- 如果你认为下跌概率大，可以把 `ask_spread` 设小一点（赶紧卖）

---

#### 4.4 order_amount — 单笔订单大小

```yaml
order_amount: "0.001"
```

**是什么？**
- 每一笔挂单买卖多少币
- 对于 BTC-USD，`0.001` = 0.001 个比特币

**按资金量推荐：**

| 你的总资金（USDC） | 杠杆 | 建议 order_amount | 说明 |
|-------------------|------|-------------------|------|
| 100 ~ 300 | 1x | 0.0005 ~ 0.001 | 试探性交易 |
| 300 ~ 1000 | 2x | 0.001 ~ 0.003 | 小资金练习 |
| 1000 ~ 5000 | 3x | 0.003 ~ 0.005 | 正常交易 |
| 5000 ~ 20000 | 3~5x | 0.005 ~ 0.01 | 资金量较大 |

**计算公式（参考）：**
```
每笔订单价值 = order_amount × 比特币价格 × 杠杆
```

例如：
- order_amount = 0.001
- BTC 价格 = 100,000 美元
- 杠杆 = 2x
- 每笔订单占用保证金 = 0.001 × 100,000 ÷ 2 = **50 USDC**

> ⚠️ **不要把所有资金都挂出去！** 至少留 50% 作为安全垫，防止价格反向波动。

---

#### 4.5 order_levels — 订单档数

```yaml
order_levels: 1
```

**是什么？**
- 机器人同时在多少个价格档位挂单

**可视化对比：**

`order_levels: 1` 时：
```
卖单: 100,500 (1单)
       ↓
中间价: 100,000
       ↓
买单:  99,500 (1单)
```

`order_levels: 3` 时：
```
卖单: 100,500 (第1单)
卖单: 101,000 (第2单, 拉开一档)
卖单: 101,500 (第3单)
       ↓
中间价: 100,000
       ↓
买单:  99,500 (第1单)
买单:  99,000 (第2单)
买单:  98,500 (第3单)
```

**建议：**
- **新手：1 档**（简单，资金集中，好管理）
- 有经验：2~3 档（覆盖更多价格区间）

**注意事项：**
- 档数越多，需要的资金越多
- 3 档 × 2 边（买+卖）= 同时挂 6 个订单

---

#### 4.6 order_level_spread — 档间价差

```yaml
order_level_spread: "0.01"
```

**是什么？**
- 当 `order_levels > 1` 时，每一档之间拉开多少百分比

**例子：**
- order_level_spread = 0.01 (1%)
- 第 1 档卖价 = 100,500
- 第 2 档卖价 = 100,500 × (1 + 1%) = 101,505
- 第 3 档卖价 = 101,505 × (1 + 1%) = 102,520

**建议：**
- 一般和 `bid_spread` / `ask_spread` 保持一致
- 如果设得太大，后面的单子离中间价太远，很难成交

---

#### 4.7 order_level_amount — 增量订单大小

```yaml
order_level_amount: "0"
```

**是什么？**
- 每一档的订单量比前一档大多少
- `0` = 每一档都一样大

**例子：**
- order_amount = 0.01
- order_level_amount = 0.005
- 第 1 档 = 0.01
- 第 2 档 = 0.01 + 0.005 = 0.015
- 第 3 档 = 0.015 + 0.005 = 0.020

**建议：**
- 新手设 `0`（所有档位一样大，简单）
- 高级用户可以用增量，让远处的单子大一点（因为远处成交概率低，一旦成交可以多赚）

---

#### 4.8 order_refresh_time — 订单刷新时间

```yaml
order_refresh_time: "30s"
```

**是什么？**
- 多久重新计算一次价格，并更新挂单

**可选值：**
- `10s` = 10 秒
- `30s` = 30 秒
- `1m` = 1 分钟
- `5m` = 5 分钟

**建议：**

| 刷新时间 | 适合场景 | 优缺点 |
|----------|----------|--------|
| 10s | 波动极大 | 紧跟市场，但费 gas |
| 30s | **推荐** | 平衡 |
| 60s | 波动小 | 省 gas，但可能滞后 |

**注意事项：**
- 每次刷新如果价格变化超过容差，机器人会撤单重挂
- 刷新太频繁会增加 gas 费（区块链交易费）

---

#### 4.9 order_refresh_tolerance_pct — 刷新容差

```yaml
order_refresh_tolerance_pct: "0"
```

**是什么？**
- 价格变化小于这个百分比时，不用撤单重挂
- `0` = 任何价格变化都刷新

**例子：**
- 容差 = 0.002 (0.2%)
- 原来挂的买单是 99,500
- 新价格算出来应该是 99,600（变化 0.1%）
- 因为 0.1% < 0.2%，所以**不刷新**，继续挂着

**建议：**
- `0` 或 `0.001`（0.1%）：适合波动小的市场
- `0.002` ~ `0.005`：减少不必要的撤单，省 gas

---

#### 4.10 filled_order_delay — 成交后冷却时间

```yaml
filled_order_delay: "60s"
```

**是什么？**
- 一旦有订单成交了，暂停多久再挂新单
- 防止机器人"上头"连续交易

**建议：**

| 冷却时间 | 风格 |
|----------|------|
| 30s | 激进，高频 |
| 60s | **推荐，平衡** |
| 120s | 保守 |
| 300s (5分钟) | 很保守，减少交易次数 |

---

#### 4.11 minimum_spread — 最小价差限制

```yaml
minimum_spread: "-100"
```

**是什么？**
- 如果当前挂单的价差小于这个值，机器人会撤掉这个单
- `-100` = 不限制（默认值）

**什么时候需要改？**
- 一般保持 `-100` 就行
- 如果你只想在价差大于 0.5% 时才做市，可以设为 `0.005`

---

#### 4.12 long_profit_taking_spread — 多头止盈价差

```yaml
long_profit_taking_spread: "0"
```

**是什么？**
- 当你持有"做多"仓位（赌涨）时，赚了多少就挂止盈单
- `0` = 不自动挂止盈单

**例子：**
- 你在 100,000 美元做多
- long_profit_taking_spread = 0.02 (2%)
- 机器人会在 102,000 美元自动挂一个卖单止盈

**建议：**
- 新手可以设 `0`（依靠 `take_profit` 统一止盈）
- 进阶用户可以配合仓位管理使用

---

#### 4.13 short_profit_taking_spread — 空头止盈价差

```yaml
short_profit_taking_spread: "0"
```

**是什么？**
- 当你持有"做空"仓位（赌跌）时，赚了多少就挂止盈单
- 用法和上面的 `long_profit_taking_spread` 一样，方向相反

---

#### 4.14 stop_loss_spread — 止损价差（已废弃）

```yaml
stop_loss_spread: "0"
```

**说明：**
- 这个参数已废弃，现在统一使用下面的 `stop_loss` 参数
- 保持 `0` 即可

---

#### 4.15 stop_loss_slippage_buffer — 止损滑点缓冲

```yaml
stop_loss_slippage_buffer: "0.005"
```

**是什么？**
- 市价止损时，允许的价格滑点
- `0.005` = 0.5%

**为什么需要这个？**
- 市价单成交时，实际成交价可能比当前价差一点
- 这个缓冲防止因为轻微滑点就止损失败

**建议：**
- 保持默认 `0.005` 即可

---

#### 4.16 time_between_stop_loss_orders — 止损单间隔

```yaml
time_between_stop_loss_orders: "60s"
```

**是什么？**
- 两次止损下单之间最少间隔多久
- 防止连续触发止损，疯狂平仓

**建议：**
- 保持默认 `60s`

---

#### 4.17 price_ceiling — 价格上限

```yaml
price_ceiling: "-1"
```

**是什么？**
- 当价格涨到这个值以上时，机器人不再挂买单
- `-1` = 不限制

**使用场景：**
- 你认为比特币超过 120,000 美元就太贵了，不想追高
- 可以设 `price_ceiling: "120000"`

---

#### 4.18 price_floor — 价格下限

```yaml
price_floor: "-1"
```

**是什么？**
- 当价格跌到这个值以下时，机器人不再挂卖单
- `-1` = 不限制

**使用场景：**
- 你认为比特币低于 80,000 美元就是底部了，不想割肉
- 可以设 `price_floor: "80000"`

---

#### 4.19 order_optimization_enabled — 订单优化

```yaml
order_optimization_enabled: false
```

**是什么？**
- 是否开启"跳价抢最优"功能
- 开启后，机器人会挂比当前最优价稍微好一点点的价格，争取排在第一位

**建议：**
- 新手：`false`（简单，不容易出错）
- 进阶：`true`（提高成交概率）

---

#### 4.20 ask_order_optimization_depth / bid_order_optimization_depth — 优化深度

```yaml
ask_order_optimization_depth: "0"
bid_order_optimization_depth: "0"
```

**是什么？**
- 订单优化时参考的深度（一般不需要改）
- 保持 `0` 即可

---

#### 4.21 stop_loss — 止损比例

```yaml
stop_loss: "0.03"
```

**是什么？**
- 这是**最重要的风控参数**
- 仓位亏到多少百分比时，自动市价平仓
- `0.03` = 亏 3% 就止损

**建议：**

| 止损值 | 风格 | 适合 |
|--------|------|------|
| 0.01 (1%) | 极保守 | 几乎不允许亏损，但可能频繁止损 |
| 0.02 (2%) | 保守 | **新手推荐** |
| 0.03 (3%) | 平衡 | **默认推荐** |
| 0.05 (5%) | 宽松 | 给波动留空间 |
| 0.10 (10%) | 很宽松 | 趋势交易，能承受较大回撤 |

> ⚠️ **没有止损 = 裸奔！** 即使其他参数设得再保守，也一定要设止损。

---

#### 4.22 take_profit — 止盈比例

```yaml
take_profit: "0.02"
```

**是什么？**
- 仓位赚到多少百分比时，自动市价平仓落袋为安
- `0.02` = 赚 2% 就止盈

**建议：**
- 做市策略靠"积少成多"，止盈可以设小一点
- `0.015` ~ `0.03` 都是合理范围

---

#### 4.23 time_limit — 持仓时限

```yaml
time_limit: "45m"
```

**是什么？**
- 一个仓位最多拿多久，超时自动平仓
- 做市的本质是"快进快出"，拿太久说明方向对你不利

**建议：**

| 时间 | 风格 |
|------|------|
| 15m | 超短线 |
| 30m | 短线 |
| 45m | **推荐** |
| 1h | 给趋势一点空间 |
| 4h | 中长线 |

---

#### 4.24 trailing_stop — 追踪止损（高级）

```yaml
# 这个参数默认不存在，需要手动添加
# trailing_stop:
#   activation_price: "0.01"
#   trailing_delta: "0.005"
```

**是什么？**
- 一种"智能止损"：盈利后自动提高止损线，保护利润

**工作原理：**
1. 仓位盈利达到 `activation_price`（如 1%）时激活
2. 记录当前盈利 - `trailing_delta`（如 0.5%）作为止损线
3. 如果盈利继续上涨，止损线跟着上移
4. 一旦盈利回落跌破止损线，自动平仓

**例子：**
- 你做多比特币，entry = 100,000
- activation_price = 0.01 (1%)
- trailing_delta = 0.005 (0.5%)

价格走势：
```
100,000 → 101,000 (+1%, 激活追踪止损, 止损线设在 +0.5% = 100,500)
101,000 → 102,000 (+2%, 止损线上移到 +1.5% = 101,500)
102,000 → 101,200 (回落到 +1.2%, 还在止损线 101,500 之上, 不触发)
101,200 → 100,800 (回落到 +0.8%, 跌破止损线 101,500! 自动平仓)
```

**建议：**
- 新手可以不设（不添加这段配置即可）
- 进阶用户可以用追踪止损保护利润

---

#### 4.25 use_bulk_orders — 批量下单

```yaml
use_bulk_orders: true
```

**是什么？**
- `true` = 把买卖单打包成一个交易提交（省 gas，推荐）
- `false` = 一笔一笔单独下

**建议：**
- **保持 `true`**
- 除非你在调试问题，想看每笔单的详细日志

---

#### 4.26 post_only — 只做 maker

```yaml
post_only: true
```

**是什么？**
- `true` = 只挂单，不吃单
- Maker = 你提供流动性，手续费通常更低甚至返佣
- Taker = 你吃掉别人的单，手续费更高

**建议：**
- **保持 `true`**，这是做市机器人的核心优势

---

#### 4.27 max_pending_tx — 最大待确认交易

```yaml
max_pending_tx: 10
```

**是什么？**
- 同时有多少个交易在等待区块链确认
- 超过这个数量后，新的交易会被丢弃

**建议：**
- 保持默认 `10`
- 如果网络很拥堵，可以适当增大

---

#### 4.28 tx_poll_interval — 交易轮询间隔

```yaml
tx_poll_interval: "2s"
```

**是什么？**
- 多久检查一次交易是否被区块链确认

**建议：**
- 保持默认 `2s`
- 网络拥堵时可以改为 `5s`

---

#### 4.29 ws_reconnect_delay — WS 重连等待

```yaml
ws_reconnect_delay: "5s"
```

**是什么？**
- WebSocket 断线后，等待多久再重连

**建议：**
- 保持默认 `5s`

---

## 配置验证

保存配置文件后，可以用辅助工具验证配置是否正确：

```bash
# 查看帮助
.\decibel-bot.exe --help

# 使用指定配置启动
.\decibel-bot.exe --config configs/config.yaml

# 或者通过环境变量
$env:DECIBEL_CONFIG = "configs/config.yaml"
.\decibel-bot.exe
```

---

## 完整配置模板

```yaml
# ==========================================
# Decibel 做市机器人 — 完整配置模板
# ==========================================

# ---------- 环境设置 ----------
env: testnet

# ---------- Decibel 账户 ----------
decibel:
  bearer_token: "你的API_TOKEN"
  api_wallet_private_key: "你的钱包私钥"
  api_wallet_address: ""
  subaccount_address: "你的子账户地址"
  market_name: "BTC-USD"
  builder_code: ""

# ---------- Aptos 节点（留空则自动根据 env 选择）----------
aptos:
  fullnode_url: ""

# ---------- 策略参数 ----------
strategy:
  # 杠杆
  leverage: 2

  # 买卖价差
  bid_spread: "0.005"
  ask_spread: "0.005"

  # 订单大小和档数
  order_amount: "0.001"
  order_levels: 1
  order_level_spread: "0.01"
  order_level_amount: "0"

  # 刷新和冷却
  order_refresh_time: "30s"
  order_refresh_tolerance_pct: "0"
  filled_order_delay: "60s"

  # 价格限制
  minimum_spread: "-100"
  price_ceiling: "-1"
  price_floor: "-1"

  # 止盈（按档位）
  long_profit_taking_spread: "0"
  short_profit_taking_spread: "0"

  # 止损相关
  stop_loss_spread: "0"
  stop_loss_slippage_buffer: "0.005"
  time_between_stop_loss_orders: "60s"

  # 订单优化
  order_optimization_enabled: false
  ask_order_optimization_depth: "0"
  bid_order_optimization_depth: "0"

  # Triple Barrier 风控
  stop_loss: "0.03"
  take_profit: "0.02"
  time_limit: "45m"

  # 追踪止损（可选，需要时取消注释）
  # trailing_stop:
  #   activation_price: "0.01"
  #   trailing_delta: "0.005"

  # 交易行为
  use_bulk_orders: true
  post_only: true
  max_pending_tx: 10
  tx_poll_interval: "2s"
  ws_reconnect_delay: "5s"
```

---

> 📘 **文档对应关系**
> - `README.md` — 项目概述和快速启动（开发者向）
> - `USAGE.md` — 小白用户完整使用教程（操作向）
> - `CONFIG_GUIDE.md` — 配置文件逐项详解（本文档，参数向）
> - `AGENTS.md` — 代码架构说明（AI/开发者向）
