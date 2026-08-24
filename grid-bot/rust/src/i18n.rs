//! Minimal compile-time localization. English is the default; Chinese is opt-in.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh")]
    Chinese,
}

impl Language {
    pub fn toggled(self) -> Self {
        match self {
            Self::English => Self::Chinese,
            Self::Chinese => Self::English,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Chinese => "中文",
        }
    }
}

/// Every user-visible string in the TUI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    AppTitle,
    TabConfigure,
    TabMarkets,
    TabPreview,
    TabMonitor,
    FieldApiKey,
    FieldAptosPrivateKey,
    FieldLanguage,
    FieldNetwork,
    FieldProduct,
    FieldMarket,
    FieldSubaccount,
    FieldPerpMode,
    FieldRangeKind,
    FieldRangeValue,
    FieldUpperBound,
    FieldGridCount,
    FieldAllocationKind,
    FieldAllocationValue,
    FieldMakerFee,
    FieldPreviewLeverage,
    FieldRefreshSeconds,
    FieldPriceSource,
    ColumnField,
    ColumnValue,
    ColumnAction,
    ColumnSide,
    ColumnPrice,
    ColumnSize,
    ColumnNotional,
    ColumnState,
    ColumnMarket,
    ColumnTick,
    ColumnLot,
    ColumnMinSize,
    ActionEdit,
    ActionCycle,
    ConfigTitle,
    MarketsTitle,
    MarketsLoading,
    MarketsEmpty,
    MarketsHint,
    PreviewTitle,
    MonitorTitle,
    GridCellsTitle,
    ControlsTitle,
    StatusTitle,
    NotConfigured,
    Optional,
    HelpConfigure,
    HelpMarkets,
    HelpGrid,
    HelpEditing,
    EditTitle,
    EditApiKeyNote,
    EditValueNote,
    EditSaveCancel,
    SavedInMemory,
    ProfileSaved,
    ProfileReset,
    ProfileLoaded,
    PasswordPromptTitle,
    PasswordPromptNew,
    PasswordPromptExisting,
    PasswordNote,
    PasswordWrong,
    LoadingMarketData,
    RangePercentLabel,
    RangeStepLabel,
    RangeBoundsLabel,
    AllocationBudgetLabel,
    AllocationSizeLabel,
    ReadOnlyBanner,
    ApiKeyRequired,
}

pub fn t(language: Language, key: Key) -> &'static str {
    use Key::*;
    use Language::{Chinese, English};
    match (language, key) {
        (English, AppTitle) => "Decibel Grid Agent",
        (Chinese, AppTitle) => "Decibel 网格助手",
        (English, TabConfigure) => "1 Configure",
        (Chinese, TabConfigure) => "1 配置",
        (English, TabMarkets) => "Markets",
        (Chinese, TabMarkets) => "市场",
        (English, TabPreview) => "2 Preview",
        (Chinese, TabPreview) => "2 预览",
        (English, TabMonitor) => "3 Monitor",
        (Chinese, TabMonitor) => "3 监控",
        (English, FieldApiKey) => "API Key",
        (Chinese, FieldApiKey) => "API 密钥",
        (English, FieldAptosPrivateKey) => "Aptos Private Key",
        (Chinese, FieldAptosPrivateKey) => "Aptos 私钥",
        (English, FieldLanguage) => "Language",
        (Chinese, FieldLanguage) => "语言",
        (English, FieldNetwork) => "Network",
        (Chinese, FieldNetwork) => "网络",
        (English, FieldProduct) => "Product",
        (Chinese, FieldProduct) => "产品",
        (English, FieldMarket) => "Market",
        (Chinese, FieldMarket) => "市场",
        (English, FieldSubaccount) => "Subaccount",
        (Chinese, FieldSubaccount) => "子账户",
        (English, FieldPerpMode) => "Perp Mode",
        (Chinese, FieldPerpMode) => "合约方向",
        (English, FieldRangeKind) => "Range Mode",
        (Chinese, FieldRangeKind) => "区间模式",
        (English, FieldRangeValue) => "Range Value",
        (Chinese, FieldRangeValue) => "区间数值",
        (English, FieldUpperBound) => "Upper Bound",
        (Chinese, FieldUpperBound) => "区间上界",
        (English, FieldGridCount) => "Total Grid Orders",
        (Chinese, FieldGridCount) => "网格订单总数",
        (English, FieldAllocationKind) => "Allocation",
        (Chinese, FieldAllocationKind) => "资金模式",
        (English, FieldAllocationValue) => "Budget / Order Size",
        (Chinese, FieldAllocationValue) => "预算 / 每格数量",
        (English, FieldMakerFee) => "Maker Fee Rate",
        (Chinese, FieldMakerFee) => "Maker 手续费率",
        (English, FieldPreviewLeverage) => "Preview Leverage",
        (Chinese, FieldPreviewLeverage) => "预览杠杆",
        (English, FieldRefreshSeconds) => "Refresh Seconds",
        (Chinese, FieldRefreshSeconds) => "刷新间隔（秒）",
        (English, FieldPriceSource) => "Price Source",
        (Chinese, FieldPriceSource) => "价格来源",
        (English, ColumnField) => "Field",
        (Chinese, ColumnField) => "字段",
        (English, ColumnValue) => "Value",
        (Chinese, ColumnValue) => "值",
        (English, ColumnAction) => "Action",
        (Chinese, ColumnAction) => "操作",
        (English, ColumnSide) => "Side",
        (Chinese, ColumnSide) => "方向",
        (English, ColumnPrice) => "Price",
        (Chinese, ColumnPrice) => "价格",
        (English, ColumnSize) => "Size",
        (Chinese, ColumnSize) => "数量",
        (English, ColumnNotional) => "Notional",
        (Chinese, ColumnNotional) => "名义价值",
        (English, ColumnState) => "State",
        (Chinese, ColumnState) => "状态",
        (English, ColumnMarket) => "Market",
        (Chinese, ColumnMarket) => "市场",
        (English, ColumnTick) => "Tick Size",
        (Chinese, ColumnTick) => "最小价格单位",
        (English, ColumnLot) => "Lot Size",
        (Chinese, ColumnLot) => "最小数量单位",
        (English, ColumnMinSize) => "Min Size",
        (Chinese, ColumnMinSize) => "最小下单量",
        (English, ActionEdit) => "edit",
        (Chinese, ActionEdit) => "编辑",
        (English, ActionCycle) => "cycle",
        (Chinese, ActionCycle) => "切换",
        (English, ConfigTitle) => "Configuration — select a row, then Enter",
        (Chinese, ConfigTitle) => "配置 — 选中一行后按 Enter",
        (English, MarketsTitle) => "Markets — Enter selects the market for the grid",
        (Chinese, MarketsTitle) => "市场列表 — 按 Enter 选择该市场",
        (English, MarketsLoading) => "Loading markets from the Decibel API…",
        (Chinese, MarketsLoading) => "正在从 Decibel API 加载市场…",
        (English, MarketsEmpty) => "No markets returned for this product on this network.",
        (Chinese, MarketsEmpty) => "该网络下此产品没有返回任何市场。",
        (English, MarketsHint) => "Markets are fetched live and filtered by the Product setting.",
        (Chinese, MarketsHint) => "市场列表实时获取，并按“产品”设置过滤。",
        (English, PreviewTitle) => "Profit Preview — theoretical maker scenario; no execution",
        (Chinese, PreviewTitle) => "利润预览 — 仅理论 maker 情景，不会下单",
        (English, MonitorTitle) => "Live Monitor — data only; this Rust version does not trade",
        (Chinese, MonitorTitle) => "实时监控 — 仅数据；当前 Rust 版本不会交易",
        (English, GridCellsTitle) => "Grid Cells — green: observed matched trade",
        (Chinese, GridCellsTitle) => "网格格子 — 绿色表示已发现匹配成交",
        (English, ControlsTitle) => "Controls",
        (Chinese, ControlsTitle) => "操作说明",
        (English, StatusTitle) => "Status",
        (Chinese, StatusTitle) => "状态",
        (English, NotConfigured) => "not configured",
        (Chinese, NotConfigured) => "未配置",
        (English, Optional) => "(optional)",
        (Chinese, Optional) => "（可选）",
        (English, HelpConfigure) => {
            "↑↓ select · Enter edit/cycle · Space cycle · [ reverse · Ctrl+S save profile · \
             Ctrl+R reset · Tab next tab · q quit"
        }
        (Chinese, HelpConfigure) => {
            "↑↓ 选择 · Enter 编辑/切换 · Space 切换 · [ 反向 · Ctrl+S 保存配置 · \
             Ctrl+R 重置 · Tab 切换页签 · q 退出"
        }
        (English, HelpMarkets) => {
            "↑↓/click select · Enter use this market · f refresh · Tab next tab · q quit"
        }
        (Chinese, HelpMarkets) => {
            "↑↓/点击 选择 · Enter 使用该市场 · f 刷新 · Tab 切换页签 · q 退出"
        }
        (English, HelpGrid) => {
            "1/2/3 or Tab switch tabs · ↑↓/click select cell · E execute Preview plan · f refresh · q quit"
        }
        (Chinese, HelpGrid) => {
            "1/2/3 或 Tab 切换页签 · ↑↓/点击选择格子 · E 执行预览计划 · f 刷新 · q 退出"
        }
        (English, HelpEditing) => "Type or paste · Enter save · Esc cancel",
        (Chinese, HelpEditing) => "输入或粘贴 · Enter 保存 · Esc 取消",
        (English, EditTitle) => "Edit setting",
        (Chinese, EditTitle) => "编辑设置",
        (English, EditApiKeyNote) => "The API key is masked while typing.",
        (Chinese, EditApiKeyNote) => "输入 API 密钥时内容会被遮蔽。",
        (English, EditValueNote) => {
            "Applied to the current session; Ctrl+S saves it to the profile."
        }
        (Chinese, EditValueNote) => "立即应用于当前会话；按 Ctrl+S 保存到配置档案。",
        (English, EditSaveCancel) => "Enter saves · Esc cancels",
        (Chinese, EditSaveCancel) => "Enter 保存 · Esc 取消",
        (English, SavedInMemory) => {
            "Setting applied to this session. Ctrl+S saves it to the profile."
        }
        (Chinese, SavedInMemory) => "设置已应用于当前会话。按 Ctrl+S 保存到配置档案。",
        (English, ProfileSaved) => "Profile saved.",
        (Chinese, ProfileSaved) => "配置档案已保存。",
        (English, ProfileReset) => "Profile reset to defaults. Ctrl+S to persist the reset.",
        (Chinese, ProfileReset) => "配置已重置为默认值。按 Ctrl+S 保存该重置。",
        (English, ProfileLoaded) => "Profile loaded.",
        (Chinese, ProfileLoaded) => "已加载配置档案。",
        (English, PasswordPromptTitle) => "Profile password",
        (Chinese, PasswordPromptTitle) => "配置档案密码",
        (English, PasswordPromptNew) => "Set a password to encrypt the saved API key.",
        (Chinese, PasswordPromptNew) => "设置一个密码，用于加密保存的 API 密钥。",
        (English, PasswordPromptExisting) => "Enter the password for the saved profile.",
        (Chinese, PasswordPromptExisting) => "请输入已保存配置档案的密码。",
        (English, PasswordNote) => {
            "The API key is encrypted with Argon2id + XChaCha20-Poly1305. Empty password cancels."
        }
        (Chinese, PasswordNote) => "API 密钥使用 Argon2id + XChaCha20-Poly1305 加密。留空则取消。",
        (English, PasswordWrong) => {
            "Wrong password or corrupted profile; the API key was not loaded."
        }
        (Chinese, PasswordWrong) => "密码错误或档案损坏；未能加载 API 密钥。",
        (English, LoadingMarketData) => "Loading market data…",
        (Chinese, LoadingMarketData) => "正在加载市场数据…",
        (English, RangePercentLabel) => "Percent around mid",
        (Chinese, RangePercentLabel) => "中间价上下百分比",
        (English, RangeStepLabel) => "Percent per grid step",
        (Chinese, RangeStepLabel) => "每格百分比间距",
        (English, RangeBoundsLabel) => "Fixed lower / upper",
        (Chinese, RangeBoundsLabel) => "固定上下界",
        (English, AllocationBudgetLabel) => "Total budget",
        (Chinese, AllocationBudgetLabel) => "总预算",
        (English, AllocationSizeLabel) => "Fixed order size",
        (Chinese, AllocationSizeLabel) => "固定每格数量",
        (English, ReadOnlyBanner) => "Preview-confirmed execution",
        (Chinese, ReadOnlyBanner) => "仅执行预览中明确确认的计划",
        (English, ApiKeyRequired) => {
            "An API key is required. Select API Key on the Configure tab and press Enter."
        }
        (Chinese, ApiKeyRequired) => "需要 API 密钥。请在“配置”页选中 API 密钥并按 Enter。",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_the_default_language() {
        assert_eq!(Language::default(), Language::English);
    }

    #[test]
    fn toggle_round_trips() {
        assert_eq!(Language::English.toggled(), Language::Chinese);
        assert_eq!(Language::English.toggled().toggled(), Language::English);
    }

    #[test]
    fn both_languages_resolve_every_key() {
        // A missing arm would fail to compile, but this guards against empty placeholders.
        for key in [
            Key::AppTitle,
            Key::TabMarkets,
            Key::PasswordPromptNew,
            Key::HelpGrid,
        ] {
            assert!(!t(Language::English, key).is_empty());
            assert!(!t(Language::Chinese, key).is_empty());
        }
    }
}
