## Plan: 移除自动 dry-run 并强制手动控制

TL;DR: 取消任何自动设置 `DRY_RUN`（无论 testnet 还是 mainnet），移除 `ALLOW_MAINNET` 相关守卫；保留从 `PACKAGE_ADDRESS` 自动推导 `PERP_ENGINE_GLOBAL_ADDRESS` 的功能；运行测试并提交补丁/PR。

**Steps**
1. 检查仓库当前状态，确认 `config/config.go`、`README.md`、`.env.example` 等文件的当前内容是否已包含预期变更（如果之前已部分修改）。
2. 若未应用：将已准备好的补丁写回 `config/config.go`，删除任何自动在 testnet/mainnet 上启用 `DRY_RUN` 的逻辑以及 `ALLOW_MAINNET` 守卫，同时保留 `PERP_ENGINE_GLOBAL_ADDRESS` 的自动推导逻辑。 (*依赖步骤 1*)
3. 格式化并运行单元测试：执行 `gofmt -w .`，然后 `go test ./...`；修复可能出现的格式或测试错误。 (*依赖步骤 2*)
4. 本地安全验证：在本机使用 `DRY_RUN=true` 并提供有效 `BEARER_TOKEN` 运行程序，确认启动日志显示 `dry_run=true` 和派生的 `perp engine address`，且不会广播交易。 Windows 示例：
   - `set DRY_RUN=true`
   - `set BEARER_TOKEN=<valid>`
   - `go run .`
   (*依赖步骤 3*)
5. 更新文档：把 `README.md` 与 `.env.example` 中关于 `DRY_RUN` 与 `ALLOW_MAINNET` 的说明更新为“必须手动指定 `DRY_RUN`（env 或 `-dry-run`）”。 (*可与第3步并行*)
6. 提交改动：创建新分支、提交修改并打开 Pull Request，PR 描述包含变更目的（安全 -> 手动控制 dry-run）、测试结果与如何复现本地验证的步骤。 (*依赖步骤 3-5*)

**Relevant files**
- [config/config.go](config/config.go) — 主修复目标（移除自动 dry-run / ALLOW_MAINNET）。
- [aptos/address.go](aptos/address.go) — 地址推导逻辑（保留）。
- [aptos/address_test.go](aptos/address_test.go) — 单元测试，需保持通过。
- [bot/bot.go](bot/bot.go) — 确保 `cfg.DryRun` 被尊重，阻止发送交易。
- [README.md](README.md) — 更新使用说明与安全注意。
- [.env.example](.env.example) — 更新示例环境变量说明。

**Verification**
1. `go test ./...` → 全部包通过。
2. `gofmt -w .` 并确认无格式差异。
3. 本地运行（见步骤 4）确认日志和不广播交易。
4. 可选：在 CI 中运行 `go test` 与 `gofmt` 作为保护。

**Decisions**
- 永远不在代码里自动对 testnet/mainnet 设定 dry-run：操作员必须通过环境变量 `DRY_RUN` 或 CLI `-dry-run` 明确指定。
- 保留 `PERP_ENGINE_GLOBAL_ADDRESS` 的自动推导（从 `PACKAGE_ADDRESS`），因为该推导是安全且提高可用性的。
- 所有高危网络提交仍由 `cfg.DryRun` 在运行时强制检查。

**Further Considerations / Questions**
1. 你是否希望我现在把修改写回工作区并创建补丁/PR？（推荐：是）
2. 是否需要我在 PR 中附上当前本地测试输出和运行日志？
3. 你希望我使用默认分支名（`fix/manual-dry-run`）还是你有偏好的分支名？
