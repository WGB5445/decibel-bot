# Decibel Grid Bot — Rust TUI + CLI

网格策略规划、监控与安全的 reconciliation-based 执行工具。支持 Decibel testnet 和 mainnet 的 **Spot**（仅 PFS 余额）和 **Perp**（中性/多头/空头模式）。

```text
grid-bot/rust
```

详细的 Testnet 验证、Shadow、Live 前检查、Mainnet 推进和故障处理请见：[运行与验证手册](运行与验证手册.md)。

## 安全设计
- Spot bulk 执行只使用 Subaccount 的 **PFS**(`spot.positions`)余额；Cross/CBS 中的 quote 不能直接用于 `place_spot_bulk_order_to_subaccount`。
- Spot 成交后的资产默认可能路由到 Cross。为避免卖出所得 USDC 不能被下一轮 bulk 买单使用,应将已有 Cross USDC 转回 PFS,并设置 `HOLD_AS_NON_COLLATERAL=true` 让未来成交继续留在 PFS。**`set_hold_as_non_collateral_for_subaccount` 只能由 subaccount owner 本人调用**(`dex_accounts::get_subaccount_signer_if_owner`,不接受 delegate),因此本工具不会代为提交这笔交易,只会提示你手动在 Decibel UI/钱包中完成。Cross→PFS 转账走 `transfer_assets_between_non_collateral_and_collateral`,可由 owner 或拥有 `ChangingCollateralFundsMovement` 权限的 delegate 签名,本工具的 API 私钥若只是 delegate 也能提交这一笔。
- TUI 的 Spot Preview/Monitor 页面会显示 Cross USDC 警告,并提示需手动设置 HOLD_AS_NON_COLLATERAL;按 `U` 打开资金设置弹窗,输入要转回 PFS 的 USDC 数量后按 Enter,只提交 Cross→PFS 转账一笔交易。不会在刷新或下单循环中自动转账,也不会尝试提交 owner-only 的 routing 设置交易。
- **Spot 引擎每轮都以当前 PFS base 余额计算缺口**；达到最小订单量时才会走有固定滑点上限的 IOC 建仓。连续失败达到阈值后会暂停并告警，不会无限支付 taker fee。详见“Spot 固定每档数量、建仓与风险门禁”。

- 每次执行前先 **reconcile**(对比期望网格与交易所实际订单)。由于 bulk API 没有 client-order-id，只要市场已存在任何订单，CLI Live 模式就不会提交 bulk replacement。
- 主网执行需要 `--confirm-mainnet MAINNET` 显式确认。
- 每个 Live/Shadow 运行写入 append-only event journal(JSON Lines)，默认状态记录在 `~/.local/share/decibel-grid/runs/<run_id>/`；受限容器可设 `DECIBEL_GRID_DATA_DIR` 覆盖根目录。
- journal 不保存原始子账户地址，仅保存不可逆 SHA3-256 指纹。

## 当前使用方式

> **真实交易入口只有常驻引擎。** `start` 启动引擎；`status`、`logs`、`attach`、`stop` 是本地瘦客户端，不会各自签名、提交订单或另起交易循环。旧 `run` 已退役，会明确报错。

### 1. 首次配置与只读检查

```bash
cd grid-bot/rust
cp .env.example .env
chmod 600 .env

cargo run -- check-key --network testnet
cargo run -- doctor --product spot --market APT/USDC --subaccount 0x...
cargo run -- reconcile --product spot --market APT/USDC --subaccount 0x...
```

真实 Spot 引擎需要 `APTOS_PRIVATE_KEY`、`SUBACCOUNT_ADDRESS` 与 `DECIBEL_API_KEY`。主网启动或停止引擎必须加 `--confirm-mainnet MAINNET`。

### 2. 常驻引擎（推荐）

```bash
# socket 就绪后立即返回；引擎继续运行
cargo run -- start --network testnet --product spot --market APT/USDC --subaccount 0x...

cargo run -- status --subaccount 0x...
cargo run -- logs --subaccount 0x...
cargo run -- logs -f --subaccount 0x...
cargo run -- attach --subaccount 0x...

# 先撤 ladder，再保留资产或尝试清仓
cargo run -- stop --subaccount 0x... --exit-mode hold
cargo run -- stop --subaccount 0x... --exit-mode liquidate
```

每个 subaccount 的控制文件：

```text
/tmp/grid-bot/<normalized-subaccount>.sock  # 单行 JSON Unix socket
/tmp/grid-bot/<normalized-subaccount>.pid   # 引擎 PID
/tmp/grid-bot/<normalized-subaccount>.log   # 默认引擎日志
```

`start` 检查 PID、socket 和账户文件锁；运行中的引擎持有同一把锁，因此同一 subaccount 不能有两个进程同时提交 bulk ladder。`status`、`logs`、`attach`、`stop` 不直接访问交易所或使用私钥。`attach` 的 Ctrl+C 只退出客户端，不会停止引擎。

引擎收到 `SIGINT` 或 `SIGTERM` 时执行和 `stop` 相同的优雅流程：先撤单，再按退出模式保留资产或清仓。不要手工删除仍在运行引擎的 PID 或 socket 文件。

### 3. systemd 与 tmux

`start` 仅用标准 `Command` 拉起内部前台 `engine`，没有 fork/双重 fork。生产环境应让 systemd 直接监管前台 `engine`：

```ini
# /etc/systemd/system/decibel-grid.service
[Unit]
Description=Decibel grid engine
After=network-online.target

[Service]
Type=simple
WorkingDirectory=/opt/decibel-grid
ExecStart=/opt/decibel-grid/decibel-grid-tui engine --network mainnet --confirm-mainnet MAINNET --product spot --market APT/USDC --subaccount 0x...
Restart=on-failure
RestartSec=5
EnvironmentFile=/opt/decibel-grid/.env

[Install]
WantedBy=multi-user.target
```

本机快速迭代可让 tmux 托管同一个前台命令：

```bash
tmux new-session -d -s decibel-grid \
  'cd /path/to/grid-bot/rust && cargo run -- engine --network testnet --product spot --market APT/USDC --subaccount 0x...'

cargo run -- status --subaccount 0x...
cargo run -- logs -f --subaccount 0x...
```

不要在 systemd/tmux 已有引擎时再次运行 `start`；锁会拒绝第二个实例。

### 4. TUI 与 Preview

```bash
cargo run -- tui
cargo run -- preview --product spot --market APT/USDC --subaccount 0x...
```

TUI 只用于配置、预览和监控：**不能提交 bulk ladder，也不会在退出时清仓。** 真实交易通过常驻引擎完成。已有引擎运行时，优先使用 `attach` 或 `status`；TUI 是一次性快照，不是引擎控制台。

`attach` 是独立的实时 ratatui 面板，通过 Unix socket 订阅类型化状态，不会调用或解析其它 CLI 命令的输出。连接后先收到完整快照，随后只接收引擎主动广播的状态变化；socket 断开后会显示“连接已断开，重连中”并以 1/2/4/8/16/30 秒退避自动重订阅：

- 顶部按层次显示 `Connected` / `Subscribed`、账户/网络/REST 快照健康度，以及 `[S]` 或 `[P]` 市场标签、订阅序号、更新时间和 mid；中部是包含 PFS 余额、完整 ladder 和事件的**单一全页快照**；底部显示控制提示、可见行范围和连接状态；
- `↑` / `↓`、`PgUp` / `PgDn`、`Home` / `End` 在同一页面内滚动全部内容；向上滚动会退出“跟随最新”，回到底部会恢复；鼠标滚轮不由应用捕获，交给宿主终端；事件只改变状态，界面最多以 30 FPS 的固定 tick 重绘，终端 resize 会在下一帧重新布局；
- `c` 将当前完整的 ladder、余额与**最新 10 条 events** 快照复制到 macOS 剪贴板；`q`、`Esc` 或 `Ctrl+C` 退出 attach；`s` 打开“停止并清仓”确认框，只有在框内按 `y` 才会发送 `stop --exit-mode liquidate` 请求，`n` / `Esc` 取消；
- 验证 panic 恢复：运行 `GRID_ATTACH_PANIC_TEST=1 cargo run -- attach --subaccount 0x...`。终端应在 panic 文本出现前退出 raw mode、alternate screen 并关闭鼠标捕获。

#### Attach TUI 实时监控

attach TUI 使用可靠的 **alternate screen** 全屏重绘，避免状态更新时文字在主终端缓冲区相互覆盖。动态重绘的 TUI 不能同时支持可靠的原生鼠标框选，因此按 `c` 会将当前完整的 ladder、余额和**最新 10 条 events**（不受屏幕大小和滚动位置影响）直接复制到 macOS 剪贴板。

```
cargo run -- attach --subaccount 0x...
```

页面布局（从上到下）：
- 连接与订阅栏：`Connected` / `Subscribed` 分别表达 socket 可用与收到完整快照；账户、网络与 REST 快照健康度独立显示
- 市场元数据：Spot 为黄色 `[S]`、Perp 为青色 `[P]`；市场名和价格加粗白色；显示本地订阅序号、最后更新时间和 mid
- PFS 余额 / PnL / reconciliation 摘要
- Ladder 表格：`Side`、`Qty (BASE)`、`Price (QUOTE)`、`Status` 明确分列；数量保留 6 位、价格保留 8 位且右对齐；BID 绿、ASK 红、价格白色加粗、取消黄、失败红
- Events 日志（按时间倒序，最新的在最上方；最多显示 10 条）

为检查网格几何，表格始终按 **BID 价格升序，然后 ASK 价格升序** 排列；标题会明确标注 `Price order: BID low→high, ASK low→high`。

滚屏操作：

| 按键 | 动作 |
|------|------|
| `↑` / `↓` | 逐行滚动 |
| `PgUp` / `PgDn` | 翻一页 |
| `Home` / `g` | 回到顶部 |
| `End` / `Shift+G` | 跳到底部 |
| `c` | 复制完整 ladder、余额和最新 10 条 events 快照到 macOS 剪贴板 |
| `s` | 打开清仓确认框 |
| `q` / `Esc` / `Ctrl+C` | 退出 attach（引擎继续运行） |

attach 在进入和退出时都会显式关闭鼠标捕获；鼠标滚轮保留给宿主终端自己的滚动、选择或快捷键行为。即使复用器仍转发滚轮报告，attach 也会忽略它们，不会为每个事件触发一次重绘。

退出 attach 会恢复原终端画面。若要保留或分享当前完整状态，请先按 `c`，再粘贴到任意文本编辑器。

attach 不会轮询 `status`，它使用长连接订阅引擎广播：收到完整快照后，仅在余额、ladder、事件有变化时收到增量推送。多个 attach 可同时连接同一个引擎互不干扰。

> **排障：** 如果 attach 无法连接，先确认引擎是否存活：
> ```
> cargo run -- status --subaccount 0x...
> ls -l /tmp/grid-bot/<normalized-subaccount>.sock
> cargo run -- logs -f --subaccount 0x...
> ```
> 旧版引擎不支持订阅推送时，请 `stop` 后重启。

#### Attach 实时订阅操作与排障

先启动**同一份新二进制**的引擎，再打开一个或多个 attach：

```bash
cargo run -- start --network testnet --product spot --market APT/USDC --subaccount 0x...

# 可以在两个终端同时运行；二者都是只读订阅者
cargo run -- attach --subaccount 0x...
cargo run -- attach --subaccount 0x...

# 日志跟随可同时运行，不会影响 attach 或引擎
cargo run -- logs -f --subaccount 0x...
```

attach 的连接语义是：发送一次 `subscribe` → 收到一份完整快照 → 仅在成交、对账、熔断、余额、ladder 或引擎阶段发生变化时收到增量更新。渲染 tick 不会调用 `status`，因此在没有状态变化时界面保持不变是正常的。

若引擎重启、socket 消失或订阅流关闭，面板会将实时连接和订阅状态标为 **断线/重连中**，但会保留最后一份完整 ladder 快照以便继续检查；它按 `1, 2, 4, 8, 16, 30` 秒间隔自动尝试重连。新引擎启动后无需重开 attach，收到新快照即恢复。如果长期无法重连，请检查：

```bash
# 引擎是否存活、socket 是否存在
cargo run -- status --subaccount 0x...
ls -l /tmp/grid-bot/<normalized-subaccount>.sock

# 引擎是否由旧二进制启动；旧引擎不支持订阅推送时请 stop 后重启
cargo run -- logs -f --subaccount 0x...
```

在 attach 里按 `s` 只会打开确认框；按 `y` 才会请求引擎停止并清仓。这是唯一会从 attach 发出的交易相关命令。正常退出 attach 使用 `q`，不要用 `s` 代替退出。

TUI 的 `U` 只会提交明确指定数量的 Cross → PFS 转账；不会自动转账，也不会设置 owner-only 的 `HOLD_AS_NON_COLLATERAL`。该 owner-only 设置应在 Decibel UI 或钱包中完成。

### 5. 其它工具

| 命令 | 用途 | 是否可能交易 |
|---|---|---|
| `check-key` | 校验 API key 格式和远端连通性 | 否 |
| `doctor` | 读取市场、计划、余额、订单并报告风险 | 否 |
| `reconcile` | 单次期望网格与实际订单对比 | 否 |
| `shadow --shadow-cycles N` | 连续模拟 reconciliation 与 journal，不签名 | 否 |
| `spot-funding-setup` | 显式配置资金路由、可选 Cross → PFS 转账 | 是，仅在明确调用时 |
| `tui` / `preview` | 配置、预览、监控 | 否 |

### 6. Spot 固定每档数量、建仓与风险门禁

Spot 首次计划会计算并持久化 `per_grid_base_size`。所有 bid/ask 档位使用同一个 base 数量；重新居中、成交后投影、区间扩展和重启恢复都不会按预算重新计算每档数量。

- `TOTAL_QUOTE_BUDGET` 和 `TOTAL_BASE_BUDGET` 只在首次确定该常量时提供上限，随后用于预算校验；若新几何超预算，引擎记录风险并拒绝替换 ladder，不会悄悄缩小某一侧。
- 引擎每次从当前 **PFS** 余额实时计算 `shortfall = plan.base_required - available_base`；不存在持久化的“已补仓”标记。
- shortfall 达到最小订单量时，使用固定滑点 cap 的 IOC 补足 base；只使用扣除完整 bid reserve 后的 PFS quote。
- IOC 未达到 `ENTRY_MIN_FILL_RATIO` 的连续失败次数达到 `ENTRY_EXIT_MAX_ATTEMPTS` 后，引擎暂停并记录 `RiskRejected`；后续对账不再自动付 taker fee，等待人工处理。
- bulk 提交前会重新读取 PFS；quote 或 base 不足时仍拒绝提交完整 ladder。

### 7. 引擎执行流程

```text
每个刷新周期：
  1. 获取市场、mid、PFS 余额、实际订单与事件状态
  2. 从固定价格几何投影当前 bid/ask；每档数量保持 per_grid_base_size
  3. 计算实时 PFS base shortfall；必要时执行受限 IOC 建仓
  4. reconcile 期望 ladder 与实际订单
  5. 若缺档且风险/资金门禁全部通过，原子替换整条 bulk ladder
  6. 记录 journal、更新本地 socket 状态并等待下个周期
```

## 研究参考

项目受以下工具设计启发，源码已下载至 `.atlas/research/`：

| 工具 | 特性 |
|------|------|
| Hummingbot V2 | Controller/Executor 分离、网格状态机、event-driven order lifecycle |
| Passivbot | Perp 网格风控（分仓敞口、realized-loss gate、reconcile-first 重启） |
| Freqtrade | 配置优先级与 JSON Schema、telegram/webhook 通知、回测 |
| OctoBot | 多交易所网格模式、paper trading 与 backtesting |

### 对本项目的关键影响

- Exchange 订单是事实来源；本地 journal 是审计和 intent 归属来源
- 没有 client-order-id 时，不自动认定与期望价格/数量相同的订单是自家订单
- 启动先进入 reconcile-only；未管理订单拒绝自动取消
- 纯函数 planning/risk core 被 preview、reconcile、execute 共享

## 功能

- 使用 `rust_decimal` 做价格、数量、费用计算，避免 `f64` 精度问题；
- Spot / Perp 网格规划；
- Neutral / Long / Short Perp 方向；
- 总预算 + 总订单数自动推导每格统一数量；
- 固定区间、上下百分比区间、每格百分比间距三种价格范围；
- Etna bulk order 限制：策略总计最多 **40 档**，且单边最多 **30 档**；
- REST 拉取 market、价格、账户概览、仓位、open orders 与 trade history；
- 将 trade history 中价格命中的网格格子标记为 `Filled`;
- TUI 中显示完整网格、当前价格、区间、仓位、可用保证金、格子状态;
- 鼠标点击或键盘上下键选中格子;
- **市场选择弹窗**:实时拉取该网络的真实市场,按 Spot / Perp 过滤,支持搜索、鼠标/键盘选择,并展示当前价格、盘口、tick / lot / min size;
- **多语言**:默认英文,可在界面内切换中文;
- **配置档案**:所有设置保存到 `~/.config/decibel-grid/profiles.json`,下次启动自动复用;API Key 用 Argon2id + XChaCha20-Poly1305 加密;
- 支持多档案（`--profile`）与一键重置;
- 不进入 TUI 也可以运行 CLI。

## 安装与构建

需要 Rust 1.95+（Aptos Rust SDK 0.6 要求）：

```bash
cd /Users/logan/code/github/wgb5445/decibel-bot/grid-bot/rust
cargo build --release
```

运行测试和检查：

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

首次构建会从 crates.io 下载依赖。

## 配置

```bash
cp .env.example .env
chmod 600 .env
```

最小 `.env`：

```dotenv
DECIBEL_API_KEY=your_decibel_api_key
NETWORK=mainnet
PRODUCT=perp
MARKET=BTC/USD

# 三种范围形式中选择一种
GRID_RANGE_PERCENT=10

# 推荐的资金/订单输入形式
GRID_TOTAL_COUNT=40
GRID_TOTAL_BUDGET=1000

PERP_GRID_MODE=neutral
PREVIEW_LEVERAGE=1
PRICE_SOURCE=prices
```

`DECIBEL_API_KEY` 必需;`SUBACCOUNT_ADDRESS` 可选,但如果设置,程序才能读取该账户的仓位、可用保证金、挂单和成交记录。

### 检查 API Key

进入 TUI 或运行网格前,可以先执行一次格式和远端有效性检查:

```bash
cargo run -- check-key --network mainnet --decibel-api-key "$DECIBEL_API_KEY"
```

也可以只通过 `.env` 提供配置:

```bash
cargo run -- check-key
```

该检查会验证 key 非空、无空白/控制字符且长度合理,然后同时检查当前网络的 REST `/markets` 和官方 WebSocket `all_market_prices` 连接。只有两者都成功才会报告 key 已被 API 接受;无效或无权限的 key 会明确返回 REST HTTP 401/403 或 WebSocket 网关错误,不会打印 key 或响应正文。

### 在 TUI 内输入 API Key

不需要先创建 `.env` 才能进入 TUI：

```bash
cargo run --
```

在 **Configure Tab** 中:

1. 用 `↑` / `↓` 选中 `API Key`,按 `Enter`;也可以鼠标点击该字段;
2. 输入或粘贴 `DECIBEL_API_KEY`;输入过程只显示圆点,不显示 key 内容;
3. 按 `Enter`:key 应用到当前会话,立即用于后续 Markets / Preview / Monitor 请求;
4. 按 `Esc`:恢复编辑前的 key,不应用修改。

默认情况下 key 只存在于内存中;只有你按 `Ctrl+S` 保存档案时才会加密写入磁盘（见下节）。

不要把 API key 直接放在可共享的终端命令行参数中;命令行可能被 shell history 或进程列表记录。

## 配置档案（Profile）

所有配置项都可以保存下来,下次启动自动复用,不必每次重新输入。

档案文件位置:

```text
~/.config/decibel-grid/profiles.json
```

该文件以 `0600` 权限写入（仅当前用户可读写）。

### 保存与重置

| 操作 | 快捷键 |
| --- | --- |
| 保存当前配置到档案 | `Ctrl+S` |
| 重置为默认值并删除该档案 | `Ctrl+R` |

按 `Ctrl+S` 后会弹出密码框。该密码用于加密 API Key:

- 密钥派生:**Argon2id**
- 加密算法:**XChaCha20-Poly1305**（AEAD）
- 每次保存都会生成新的随机 salt 和 nonce
- 密码留空则取消保存

除 API Key 外的配置项以明文 JSON 保存,便于你直接查看和手工编辑;只有 API Key 是加密的。

### 下次启动

启动时会自动加载档案中的配置项。如果档案里存有加密的 API Key,会弹出密码框要求解密:

- 输入正确密码:加载 API Key 并继续;
- 密码错误:提示 `密码错误或档案损坏`,**不会**返回错误的明文（AEAD 认证失败即拒绝）;
- 按 `Esc`:跳过加载,你仍可手工输入 key 或从 `.env` 读取。

优先级为:

```text
CLI 参数 > 环境变量 / .env > 已保存档案 > 内置默认值
```

### 多档案

用 `--profile` 或 `GRID_PROFILE` 区分不同配置组合:

```bash
# 主网合约档案
cargo run -- --profile mainnet-perp

# 测试网现货档案
cargo run -- --profile testnet-spot
```

每个档案独立保存,互不影响。`Ctrl+R` 只重置当前档案。

## 多语言

界面支持英文和中文,**默认英文**。

在 Configure Tab 选中 `Language` 字段,按 `Enter` 或 `Space` 即可切换:

```text
English  ⇄  中文
```

语言设置会随 `Ctrl+S` 一起保存到档案,下次启动自动应用。

## 智能启动规则

程序会根据命令和参数自动选择合适界面：

| 启动方式 | 行为 |
| --- | --- |
| `cargo run --` | 打开 **Configure Tab**。可在 Tab 内编辑 API Key、网络、市场、产品、预算、范围和监控参数。 |
| `cargo run -- tui` | 强制打开 **Configure Tab**，即使 `.env` 或 CLI 已有完整网格参数。 |
| `cargo run -- preview ...` | 直接打开 **Preview Tab**，显示每个格子、扣 maker fee 后的理论捕获、预算/保证金情况。 |
| `cargo run -- --range-percent 10 --grid-count 40 --total-budget 1000 ...` | 检测到**完整**价格范围和资金配置，直接打开 **Monitor Tab**。仅提供部分参数仍打开 Configure Tab。 |
| `cargo run -- run ...` | 不打开 TUI；持续将快照输出到 stdout，便于 `tmux`、日志重定向和守护进程管理。 |

默认无参数行为不再因为缺少区间或预算而立即退出；它会显示 TUI 操作仪表盘。真正读取市场数据时仍需要 `DECIBEL_API_KEY`。

### TUI 操作流程

推荐顺序是「先选产品 → 再从真实市场列表里选市场 → 预览 → 监控」:

1. 启动进入 **Configure Tab**,先用 `↑` / `↓` 选中 `Product`,按 `Enter` 在 `Spot` / `Perp` 间切换;
2. 选中 `Market` 字段按 `Enter`，或在任意页面按 `m` 打开**市场选择弹窗**；
3. 程序调用 Decibel API 拉取该网络下的真实市场，并按当前 `Product` 过滤；输入任意文字可本地搜索；
4. 用 `↑` / `↓` 或鼠标点击选中市场，右侧立即展示当前中间价、真实盘口、tick / lot / min size；再次点击该行或按 `Enter` 应用；
5. 回到 Configure 补齐总订单数、预算和价格范围；
6. 按 `2` 到 **Preview Tab**，查看实时网格与扣费后的理论收益；
7. 按 `3` 到 **Monitor Tab**，持续刷新市场、账户、成交历史与网格格子；
8. 满意后按 `Ctrl+S` 保存档案，下次启动直接复用；
9. TUI / Monitor **不会提交交易**；`run` 已退役。真实交易使用常驻 `start` 引擎，实时查看使用 `attach` 或 `status`。

### 市场选择弹窗

弹窗是一个交易终端式的实时面板，而不是独立页面：

```text
┌ Search: btc              | Product: Perp | Network: mainnet ┐
├ Markets ─────────────────┬ Live market detail ─────────────────┤
│ * BTC/USD   tick 0.1     │ BTC/USD | Mid: 71,547.4              │
│   ETH/USD   tick 0.01    │ Tick / Lot / Min size                │
│   ...                    │                                      │
│                          │ ASKS (best first)                     │
│                          │ price            size                │
│                          │ ...                                  │
│                          │ BIDS (best first)                    │
└──────────────────────────┴──────────────────────────────────────┘
```

左侧只显示当前 `Product` 的市场；切换 Spot / Perp 或 Network 后会自动重新获取。右侧展示：

- Perp 使用官方 WebSocket `all_market_prices` 的当前中间价；Spot 使用 `depth:<market>:1` 的 best bid / best ask 中间价；
- 官方 WebSocket `depth:<market>:1` 前 8 档 Ask / Bid；
- Tick Size、Lot Size、Min Size；
- 当前选中的市场名称。

操作：

| 操作 | 键盘 | 鼠标 |
| --- | --- | --- |
| 打开 | `m`，或 Market 字段按 `Enter` | 点击 Market 字段 |
| 搜索 | 直接输入 | — |
| 清除搜索 | `Backspace` | — |
| 切换选中市场 | `↑` / `↓` | 点击列表行 |
| 应用市场 | `Enter` | 再次点击已选中的列表行 |
| 关闭 | `Esc` | — |
| 立即刷新盘口 | `f` | — |

市场列表按 `(网络, 产品)` 缓存。盘口和价格每 2 秒刷新；按 `f` 立即更新。

## CLI

### 单次预览

```bash
cargo run -- preview \
  --network mainnet \
  --decibel-api-key "$DECIBEL_API_KEY" \
  --product perp \
  --market BTC/USD \
  --range-percent 10 \
  --grid-count 40 \
  --total-budget 1000 \
  --perp-mode neutral \
  --price-source prices
```

输出每一格最终经过 tick / lot 对齐后的：

```text
BID  70,000.0 × 0.0012  Planned
ASK  73,000.0 × 0.0012  Filled
```

### 不进入 TUI 的持续监控

```bash
cargo run -- run \
  --network mainnet \
  --decibel-api-key "$DECIBEL_API_KEY" \
  --product perp \
  --market BTC/USD \
  --range-percent 10 \
  --grid-count 40 \
  --total-budget 1000 \
  --refresh-seconds 3 \
  --price-source prices
```

每个刷新周期会重新获取价格、账户和成交历史，并输出重新计算后的网格状态。使用 `Ctrl-C` 停止。

### TUI

```bash
cargo run -- tui \
  --network mainnet \
  --decibel-api-key "$DECIBEL_API_KEY" \
  --subaccount 0xYOUR_SUBACCOUNT \
  --product perp \
  --market BTC/USD \
  --range-percent 10 \
  --grid-count 40 \
  --total-budget 1000 \
  --perp-mode neutral \
  --refresh-seconds 3 \
  --price-source prices
```

发布模式：

```bash
./target/release/decibel-grid-tui tui ...
```

## TUI 操作

TUI 现在是三页主 Tab；市场选择为叠加弹窗，不会打断当前页面:

```text
[1 Configure] [2 Preview] [3 Monitor]

Market picker: press m, or focus Market and press Enter
```

### Configure Tab

配置页采用左右分栏，类似网页表单：

```text
┌─ 配置表单 ────────────┬─ 该设置的含义 ──────┐
│ Field   Value  Action │ 解释文字            │
│ ...                   │                     │
│ Range ± (%)  10  edit │ 例：10 表示当前中间 │
│ ...                   │ 价的 [90%, 110%]，  │
│                       │ 不是美元价格。      │
│                       │                     │
│                       │ Current: 10         │
│                       │                     │
│                       │ Simulation with     │
│                       │ current mid ...     │
│                       │ 40 levels (20+20)   │
│                       │ Estimated net ...   │
└───────────────────────┴─────────────────────┘
```

右栏会随光标所在字段实时变化，包含三部分：

1. **含义解释**：这个字段到底是什么、单位是什么；
2. **Current**：当前值；
3. **Simulation**：用当前配置和实时中间价算出的档位数、扣费后净捕获、资金占用。

字段名会随模式自动改写，不再是含糊的 `Range Value`：

| Range Mode | 字段显示为 | 含义 |
| --- | --- | --- |
| 中间价百分比 | `Range ± (%)` | 填 `10` = 中间价的 `[90%, 110%]` |
| 每格百分比 | `Step (%)` | 填 `0.5` = 每格约相差 0.5% |
| 固定上下界 | `Lower Price` + `Upper Price` | 填报价，如 `65000` |

`Budget / Order Size` 同理，会显示为 `Total Budget` 或 `Order Size`。

配置页使用上下方向键选择字段:

| 操作 | 键盘 | 鼠标 |
| --- | --- | --- |
| 上一个字段 | `↑` | 点击字段行 |
| 下一个字段 | `↓` | 点击字段行 |
| 编辑文本字段 | `Enter` | 点击可编辑字段 |
| 保存文本字段 | `Enter` | — |
| 取消编辑 | `Esc` | — |
| 切换枚举字段 | `Space` / `Enter` | 点击字段 |
| 反向切换枚举字段 | `[` | — |
| 切换下一个 Tab | `Tab` / `→` | 点击 Tab |
| 切换上一个 Tab | `Shift+Tab` / `←` | 点击 Tab |

配置字段包括:

- API Key;
- Language:`English` / `中文`,默认英文;
- Network:`testnet` / `mainnet`;
- Product：`spot` / `perp`；
- Market；
- Subaccount；
- Perp Mode:`neutral` / `long` / `short`;**仅 Product=perp 时显示**;
- Range Mode:中间价百分比、每格百分比、固定上下界;
- `Range ± (%)` / `Step (%)` / `Lower Price`（随 Range Mode 自动改名）;
- `Upper Price`（仅固定上下界模式显示）;
- Total Grid Orders;
- Allocation:Total Budget / Fixed Order Size;
- `Total Budget` 或 `Order Size`（随 Allocation 自动改名）;
- Maker Fee Rate；
- Preview Leverage；**仅 Product=perp 时显示**；
- Refresh Seconds；
- Price Source：`prices` / `depth`。

### Preview Tab

Preview 页显示：

- 当前市场与中间价；
- 实际生成的 Bid / Ask 格子；
- 每格价格、数量、名义价值；
- 配对数量；
- 毛捕获、Maker Fee、扣费后净捕获；
- Spot 资金需求或 Perp 保证金估算；
- 成交历史命中的 `Filled` 格子。

### Monitor Tab

Monitor 页按刷新间隔持续更新市场、账户、仓位、订单和成交状态。当前 Rust 版本的 Monitor 仍是只读监控，不会提交交易。

### 通用操作

| 操作 | 键盘 | 鼠标 |
| --- | --- | --- |
| Configure | `1` / `Esc` | 点击 Configure Tab |
| 打开市场弹窗 | `m`，或在 Market 字段按 `Enter` | 点击 Market 字段 |
| Preview | `2` / `p` | 点击 Preview Tab |
| Monitor | `3` / `r` | 点击 Monitor Tab |
| 保存配置档案 | `Ctrl+S` | — |
| 重置配置档案 | `Ctrl+R` | — |
| 立即刷新 | `f` | — |
| 选择网格格子 | `↑` / `↓` | 点击格子行 |
| 退出 | `q` | — |

格子状态：

| 状态 | 含义 |
| --- | --- |
| `Planned` | 当前计算出的网格档位，未从这次 REST 读取的成交历史里发现匹配成交。 |
| `Selected` | 用户通过键盘/鼠标选中的格子。 |
| `Filled` | `trade_history` 返回了价格与该档 tick 对齐匹配的成交。 |

### 关于 `Filled` 的准确性

`Filled` 是一个监控提示：当前第一版按 `/trade_history` 最近 100 条中与本次价格格子匹配的成交标记。它不等同于链上永久订单生命周期，也不能区分“旧运行期成交”与“本次运行期成交”。

原生交易执行版会增加：启动时间水位、bulk sequence、bulk resting vectors、order ID 到格子的映射和持久化状态文件，使 `Resting / Filled / Replaced / Cancelled` 更精确。

## 网格参数

### 总订单数

`--grid-count` 是 Bid + Ask 的合计数量：

| 参数 | Neutral 结果 |
| ---: | --- |
| `10` | 5 Bid + 5 Ask |
| `40` | 20 Bid + 20 Ask |
| `80` | 策略总计 40 档；单边仍不得超过 30 档 |

Long Perp 将总数都作为 Bid，Short Perp 将总数都作为 Ask。

### 总预算

```bash
--total-budget 1000
```

- **Spot**：约 50% 预留给 Bid 所需 quote（包含 maker fee buffer），另约 50% 按 Ask 挂单价值反推所需 base；
- **Perp**：当作保守保证金预算，按较大一侧名义价值、预览杠杆与 fee buffer 推导每格大小；
- 数量会向下对齐 lot size，所以实际使用通常略低于预算；
- 预算不足以达到 `min_size` 时，命令会失败，而不是生成不可提交订单。

也可以明确每格大小：

```bash
--order-size 0.001
```

但 `--order-size` 和 `--total-budget` 不可同时使用。

### 价格范围

三选一：

```bash
# 固定边界；当前中间价必须在其中
--lower-price 65000 --upper-price 80000

# 中间价上下各 10%
--range-percent 10

# 每格复合间距约 0.5%
--grid-step-percent 0.5
```

`--grid-step-percent 0.5 --grid-count 40` 的第 `i` 档为：

```text
Bid_i = mid × (1 - 0.5%)^i
Ask_i = mid × (1 + 0.5%)^i
```

## 价格源

```bash
--price-source prices  # 默认：/prices mid_px，适合 API /depth 对该市场 404 的情况
--price-source depth   # /depth 最优 bid/ask 的平均值
```

`prices` 并不是实时订单簿。若要用于未来实盘执行，优先使用正常可用的 `depth`，否则可能因为价格滞后造成 POST_ONLY 订单拒绝或不符合预期的报价。

## 与 Python / Go 版本的关系

- `../python`：当前可通过 SDK 提交 Spot/Perp bulk order 的网格工具；
- `../../market-maker/go`：成熟的 Perp market maker，包含 Aptos 原生签名、批量订单替换、风控和通知；
- 本 Rust 版本：CLI/TUI 网格监控和纯 Rust 规划核心，执行功能将以 Etna 的 `perpdex-sdk` / Aptos 原生签名接口接入，并在 testnet bulk-order 提交测试完成后开放。
