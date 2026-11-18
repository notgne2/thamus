use serde::{Deserialize, Serialize};

pub mod dream;
pub mod infusion;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SmartsConfig {
    /// Configure dreaming
    pub dreaming: dream::DreamingConfig,
    /// Configure infusions
    pub infusion: infusion::InfusionConfig,
}
