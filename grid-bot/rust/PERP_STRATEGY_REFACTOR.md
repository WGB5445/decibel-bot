# Perp Strategy Refactor Plan

## 问题

当前 Perp 策略只实现了一种固定网格：方向性 ladder（`PerpMode::Long` / `Short` / `Neutral`）。
每次 bid fill 后，风控只会裁剪 bid 层，不会自动补对应 take-profit ask。
这不是真正的网格做市策略，不适合认为"bid 成交后应该增加 ask"的用户。

## 目标

把 Perp 策略改成可选的：

| 策略模式 | CLI 参数 | 行为 |
|---|---|---|
| 方向性 ladder（当前实现） | `--perp-mode long\|short\|neutral` | 保持当前逻辑 |
| 旋转网格（新实现） | `--perp-mode rotate` | 每次 fill 补对侧止盈单 |
| 未来可扩展 | `--perp-mode grid` | Neutral 旋转网格 |

## 架构改动

### 1. 策略 trait

`strategy/perp/trait.rs`:

```rust
pub trait PerpGridStrategy {
    fn build_initial_plan(&self, config, market, price) -> GridPlan;
    fn on_fill(&mut self, fill: &PerpFill, market: &Market) -> Vec<LevelAdjustment>;
    fn build_replacement_plan(&self, snapshot: &MonitorSnapshot) -> GridPlan;
    fn target_position(&self) -> Decimal;
    fn risk_check(&self, position: Decimal, plan: &GridPlan) -> Result<()>;
}
```

### 2. DirectionalLadder（当前实现 → 保留）

`strategy/perp/directional.rs`:

- 提取当前 `build_perp_plan` + `apply_perp_risk_trim` + `finalize_perp_executable_plan` 中的逻辑
- 保持不变

### 3. RotatingGrid（新实现）

`strategy/perp/rotation.rs`:

```
struct InventoryLot {
    entry_side: Side,      // 成交时的 side
    entry_price: Decimal,  // 成交价格
    size: Decimal,         // 成交量
    exit_side: Side,       // 应挂的对手 side
    exit_price: Decimal,   // 止盈价格（下一档 grid step）
    status: LotStatus,     // Open / ExitResting / Closed
}

struct RotatingGridState {
    config: GridConfig,
    pinned_prices: Vec<Decimal>,  // 原始 grid 价格列表
    lots: Vec<InventoryLot>,      // 当前持仓 lot
    available_bids: Vec<Decimal>, // 可挂的 bid 价格
    available_asks: Vec<Decimal>, // 可挂的 ask 价格
}
```

**关键逻辑：**

bid fill：
```
fill @ price P, size S
→ 创建 InventoryLot { entry: Bid, entry_price: P, exit: Ask, exit_price: next_ask_price(P), status: ExitResting }
→ 减少 available_bids（一个 slot 被填了）
→ 提交 exit ask @ exit_price
```

ask fill：
```
fill @ price P, size S
→ 查找对应 InventoryLot（entry Bid @ < P）
→ 标记 Closed
→ 恢复 available_bids slot
```

**推导待提交 ladder：**
```
ladder = available_bids.map(price → Bid level)
       + active_exit_asks.map(price → Ask level)
       + available_asks.map(price → Ask level)
       + active_exit_bids.map(price → Bid level)
```

### 4. CLI 参数

`--perp-mode` 增加 `rotate` 选项：

```rust
pub enum PerpMode {
    Long,
    Short,
    Neutral,
    Rotate,
}
```

### 5. journal 持久化

新增 `RotatingGridState` 保存在 `PerpRuntimeState` 中：

```rust
pub struct PerpRuntimeState {
    pub strategy_type: PerpStrategyType, // Directional | Rotating
    pub rotating_state: Option<RotatingGridState>,
    // ... 现有字段
}
```

## 实施步骤

### Step 1: 提取 DirectionalLadder
- 新建 `strategy/perp/directional.rs`
- 从 `planning.rs`、`risk.rs`、`runtime.rs` 提取当前逻辑
- 实现 `PerpGridStrategy` trait

### Step 2: 实现 `RotatingGrid`
- 新建 `strategy/perp/rotation.rs`
- 实现 `InventoryLot`、`RotatingGridState`
- 实现 fill → rotate 逻辑
- 实现推导重建 ladder 逻辑
- 实现风控（限制 entry capacity，不轻易裁剪 exit orders）

### Step 3: CLI 支持
- `--perp-mode` 增加 `rotate`
- TUI 展示策略类型

### Step 4: journal 持久化
- `PerpRuntimeState` 增 `strategy_type` + `rotating_state`
- Replay 支持两种策略

### Step 5: 测试
- 方向性 ladder 现有测试全部保留
- 旋转网格新增测试：
  - bid fill → 补 ask
  - ask fill → 恢复 bid slot
  - 多个 fill 时的 ladder 推导
  - 风控裁剪 entry capacity 但保留 exit orders

## 预期影响范围

| 文件 | 改动 |
|---|---|
| `src/strategy/perp/mod.rs` | 新增 trait, directional, rotation module |
| `src/strategy/perp/trait.rs` | 新增 |
| `src/strategy/perp/directional.rs` | 新增（从现有拆出） |
| `src/strategy/perp/rotation.rs` | 新增 |
| `src/strategy/perp/planning.rs` | Directional 部分的逻辑移到 directional.rs |
| `src/strategy/perp/risk.rs` | Directional 部分移到 directional.rs |
| `src/strategy/perp/runtime.rs` | 分发到对应策略 |
| `src/journal.rs` | `PerpRuntimeState` 新增字段 |
| `src/cli/settings.rs` | `PerpMode` 增 `Rotate` |
| `src/engine/mod.rs` | 根据 strategy_type 调用不同 plan 生成逻辑 |
| `src/control.rs` | `EngineStatus` 展示策略类型 |
| `src/attach_tui.rs` | 展示策略类型 |