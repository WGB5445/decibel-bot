//! Offline grid planning and scenario simulation — zero network I/O.

pub mod cycle;
pub mod scenario;

pub use cycle::{OfflineCycleInput, OfflineCycleOutput, run_offline_cycle};
pub use scenario::{
    Scenario, ScenarioStepExpect, parse_scenario, simulate_scenario, simulate_scenario_from_str,
};
