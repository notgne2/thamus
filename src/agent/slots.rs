use crate::{CommonResources, agent::AgentConfig, db, providers::llm::LlmConfig};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SlotsConfig {
    pub agent_slots: Vec<u8>,
    pub dream_slots: Vec<u8>,
}

pub async fn release_slot(common: &CommonResources, id_slot: u8) -> Result<()> {
    db::slots::release_slot(&common.pool, &common.lock_notifier, id_slot).await?;
    Ok(())
}

pub async fn get_slot(
    common: &CommonResources,
    llm_config: &LlmConfig,
    sub_config: &AgentConfig,
    id: &str,
    is_dream: bool,
) -> Result<u8> {
    if (sub_config.slots.agent_slots.is_empty() && !is_dream)
        || (sub_config.slots.dream_slots.is_empty() && is_dream)
    {
        debug!("No slots configured for this instance type, assigning 0 without checking");
        return Ok(0);
    }

    let (slot, last_was_dream, last_instance_id) = db::slots::acquire_slot(
        &common.pool,
        &common.lock_notifier,
        &sub_config.slots.agent_slots,
        &sub_config.slots.dream_slots,
        is_dream,
        id,
    )
    .await
    .context("Failed to acquire slot")?;

    trace!("Acquired slot {slot}");

    if let (Some(last_instance_id), Some(last_was_dream)) = (last_instance_id, last_was_dream)
        && (last_instance_id != id || last_was_dream != is_dream)
    {
        trace!(
            "Saving contents previous slot {slot} inhabitant ({last_instance_id}, {last_was_dream})"
        );

        let client = &common.http;
        client
            .post(format!(
                "{}/slots/{}?action=save",
                llm_config.base_url,
                slot
            ))
            .json(&serde_json::json!({"filename": format!("{}_{}.bin", last_instance_id, if last_was_dream { "dream" } else { "agent" })}))
            .send()
            .await
            .context("Failed to save previous inhabitant of slot")?;

        trace!("Loading last save/contents of this instance into {slot}");

        client
            .post(format!(
                "{}/slots/{}?action=restore",
                llm_config.base_url,
                slot
            ))
            .json(&serde_json::json!({"filename": format!("{}_{}.bin", id, if is_dream { "dream" } else { "agent" })}))
            .send()
            .await
            .context("Failed to load current instance contents into slot")?;
    }

    Ok(slot)
}
