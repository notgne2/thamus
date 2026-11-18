use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Notify;
use tracing::{instrument, trace};

const RELEASE_SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

#[instrument(level = "debug", skip_all, fields(is_dream=is_dream), ret)]
pub async fn acquire_slot(
    pool: &SqlitePool,
    released: &Notify,
    agent_slots: &[u8],
    dream_slots: &[u8],
    is_dream: bool,
    instance_id: &str,
) -> anyhow::Result<(u8, Option<bool>, Option<String>)> {
    let slots: Vec<i16> = {
        let src = if is_dream { &dream_slots } else { &agent_slots };
        src.iter().map(|&s| i16::from(s)).collect()
    };

    trace!(
        "Choosing one of {:?} slots for is_dream={:?}",
        slots, is_dream
    );

    // Seed rows for every slot in either array — slots are global, not per-kind.
    let all: Vec<i16> = agent_slots
        .iter()
        .chain(dream_slots.iter())
        .map(|&s| i16::from(s))
        .collect();
    // `WHERE true` is required: SQLite parses `ON CONFLICT` after a SELECT as
    // the select's join condition, so the upsert clause is a syntax error without it.
    sqlx::query(
        "INSERT INTO llm_slot_leases (slot_id)
             SELECT DISTINCT value FROM json_each(?1)
             WHERE true
             ON CONFLICT DO NOTHING",
    )
    .bind(serde_json::to_string(&all)?)
    .execute(pool)
    .await
    .context("Failed to seed slot leases")?;

    let slots_json = serde_json::to_string(&slots)?;

    loop {
        let notified = released.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // A single UPDATE ... FROM (candidate CTE) is atomic in SQLite, so the
        // pick and the claim cannot interleave with another waiter. A separate
        // SELECT then UPDATE would let two waiters read the same free slot,
        // with one losing the race and retrying for nothing.
        let claimed: Option<(i16, Option<bool>, Option<String>)> = sqlx::query_as(
            "
                    WITH candidate AS (
                        SELECT slot_id, last_dream, last_instance_id
                          FROM (
                                SELECT slot_id,
                                       last_dream,
                                       last_instance_id,
                                       acquired_at,
                                       (last_instance_id IS ?3 AND last_dream IS ?1) AS affinity
                                  FROM llm_slot_leases
                                 WHERE slot_id IN (SELECT value FROM json_each(?2))
                                   AND NOT in_use
                               )
                         ORDER BY
                           -- 1. affinity: prefer the slot this same
                           --    (instance_id, is_dream) held most recently
                           affinity DESC,
                           -- 2. among affinity matches: MOST recently acquired
                           CASE WHEN affinity THEN acquired_at END DESC,
                           -- 3. otherwise: never acquired first, then least recently used
                           CASE WHEN NOT affinity THEN acquired_at END ASC,
                           -- 4. deterministic tiebreak
                           slot_id
                         LIMIT 1
                    )
                    UPDATE llm_slot_leases
                                           SET in_use = 1,
                                               acquired_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                                               prev_dream = llm_slot_leases.last_dream,
                                               prev_instance_id = llm_slot_leases.last_instance_id,
                                               last_dream = ?1,
                                               last_instance_id = ?3
                                          FROM candidate
                                         WHERE llm_slot_leases.slot_id = candidate.slot_id
                                        RETURNING slot_id, prev_dream, prev_instance_id
                                ",
        )
        .bind(is_dream)
        .bind(&slots_json)
        .bind(instance_id)
        .fetch_optional(pool)
        .await
        .context("Failed to acquire slot lease")?;

        if let Some((slot_id, last_dream, last_instance_id)) = claimed {
            return Ok((
                u8::try_from(slot_id).context("Slot ID way out of realistic range")?,
                last_dream,
                last_instance_id,
            ));
        }

        tokio::select! {
            () = &mut notified => {}
            () = tokio::time::sleep(RELEASE_SIGNAL_TIMEOUT) => {}
        }
    }
}

#[instrument(level = "debug", skip(pool, released))]
pub async fn release_slot(pool: &SqlitePool, released: &Notify, slot: u8) -> sqlx::Result<()> {
    sqlx::query("UPDATE llm_slot_leases SET in_use = 0 WHERE slot_id = ?1")
        .bind(i16::from(slot))
        .execute(pool)
        .await?;

    released.notify_waiters();

    Ok(())
}
