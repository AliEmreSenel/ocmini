use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct HookDeliveryBudget {
    pub retention_ms: u64,
    pub count: usize,
    pub bytes: usize,
    pub total_count: usize,
    pub total_bytes: usize,
}
/// Notification routing is resolved before the domain transaction. Store only
/// inserts these opaque delivery intents under the supplied budgets.
pub struct OutboxIntent {
    pub subscription_key: String,
    pub event_id: String,
    pub directory: String,
    pub event_json: String,
    pub budget: HookDeliveryBudget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDeliveryRecord {
    pub subscription_key: String,
    pub event_id: String,
    pub directory: String,
    pub event_json: String,
    pub event_bytes: usize,
    pub attempts: u32,
    pub next_attempt: i64,
    pub expires: i64,
    pub state: String,
    pub lease_owner: Option<String>,
    pub lease_until: Option<i64>,
}

impl Store {
    pub fn persist_outbox_tx(
        tx: &Transaction<'_>,
        intents: &[OutboxIntent],
    ) -> Result<Vec<String>, OperationError> {
        let mut failures = Vec::new();
        for intent in intents {
            if !Self::persist_hook_delivery_tx(
                tx,
                &intent.subscription_key,
                &intent.event_id,
                &intent.directory,
                &intent.event_json,
                &intent.budget,
            )? {
                failures.push(format!(
                    "observer outbox quota exceeded for subscription {}",
                    intent.subscription_key
                ));
            }
        }
        Ok(failures)
    }

    /// Inserts a durable hook delivery within an existing domain transaction.
    /// Returns Ok(true) if inserted or already present, Ok(false) if rejected due to outbox quota.
    pub fn persist_hook_delivery_tx(
        tx: &Transaction<'_>,
        subscription_key: &str,
        event_id: &str,
        directory: &str,
        event_json: &str,
        budget: &HookDeliveryBudget,
    ) -> Result<bool, OperationError> {
        let now = now_ms();
        // Expiry is global so renamed/disabled registrations cannot leave
        // abandoned namespaces consuming disk indefinitely.
        tx.execute(
            "DELETE FROM hook_delivery WHERE expires <= ?1",
            params![now],
        )?;

        // Deduplication: check if (subscription_key, event_id) already exists
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM hook_delivery WHERE subscription_key = ?1 AND event_id = ?2)",
                params![subscription_key, event_id],
                |r| r.get(0),
            )
            ?;
        if exists {
            return Ok(true);
        }

        // Check quota: include exhausted records until retention
        let (current_count, current_bytes): (i64, i64) = tx
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(event_bytes), 0) FROM hook_delivery WHERE subscription_key = ?1",
                params![subscription_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            ?;

        let event_bytes = event_json.len();
        let (total_count, total_bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*),COALESCE(SUM(event_bytes),0) FROM hook_delivery",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if (current_count as usize) >= budget.count
            || (current_bytes as usize).saturating_add(event_bytes) > budget.bytes
            || (total_count as usize) >= budget.total_count
            || (total_bytes as usize).saturating_add(event_bytes) > budget.total_bytes
        {
            return Ok(false);
        }

        let expires = now.saturating_add(budget.retention_ms as i64);
        tx.execute(
            "INSERT INTO hook_delivery(subscription_key, event_id, directory, event_json, event_bytes, attempts, next_attempt, expires, state)
             VALUES(?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, 'pending')",
            params![
                subscription_key,
                event_id,
                directory,
                event_json,
                event_bytes as i64,
                now,
                expires,
            ],
        )
        ?;

        Ok(true)
    }

    /// Claim ONE pending or lease-expired delivery for a subscription.
    /// Claims atomically in a short transaction with a lease token.
    /// Counts attempts on claim to avoid crash loops, marking exhausted if max_attempts is reached.
    pub fn claim_hook_delivery(
        &self,
        subscription_key: &str,
        lease_ms: i64,
        max_attempts: u32,
    ) -> Result<Option<HookDeliveryRecord>, OperationError> {
        self.transaction(|tx| {
            let now = now_ms();
            let lease_until = now.saturating_add(lease_ms);

            // Clean expired records
            tx.execute(
                "DELETE FROM hook_delivery WHERE expires <= ?1",
                params![now],
            )
            ?;

            let candidate: Option<HookDeliveryRecord> = tx
                .query_row(
                    "SELECT event_id, directory, event_json, event_bytes, attempts, next_attempt, expires, state
                     FROM hook_delivery
                     WHERE subscription_key = ?1
                       AND (state = 'pending' OR (state = 'running' AND lease_until <= ?2))
                       AND next_attempt <= ?2
                       AND expires > ?2
                     ORDER BY next_attempt ASC
                     LIMIT 1",
                    params![subscription_key, now],
                    |r| Ok(HookDeliveryRecord {
                        subscription_key: subscription_key.to_owned(), event_id: r.get(0)?, directory: r.get(1)?,
                        event_json: r.get(2)?, event_bytes: r.get::<_, i64>(3)? as usize, attempts: r.get(4)?,
                        next_attempt: r.get(5)?, expires: r.get(6)?, state: r.get(7)?, lease_owner: None, lease_until: None,
                    }),
                )
                .optional()
                ?;

            let Some(mut delivery) = candidate else {
                return Ok(None);
            };
            if delivery.attempts >= max_attempts {
                tx.execute(
                    "UPDATE hook_delivery
                     SET attempts = ?1,
                         state = 'exhausted',
                         lease_owner = NULL,
                         lease_until = NULL
                     WHERE subscription_key = ?2 AND event_id = ?3",
                    params![delivery.attempts, subscription_key, delivery.event_id],
                )
                ?;
                return Ok(None);
            }

            delivery.attempts = delivery.attempts.saturating_add(1);

            let lease_token = format!("{}:{}", self.instance, crate::types::hex_id("lst_"));

            tx.execute(
                "UPDATE hook_delivery
                 SET state = 'running',
                     attempts = ?1,
                     lease_owner = ?2,
                     lease_until = ?3
                 WHERE subscription_key = ?4 AND event_id = ?5",
                params![
                    delivery.attempts,
                    lease_token,
                    lease_until,
                    subscription_key,
                    delivery.event_id
                ],
            )
            ?;

            delivery.state = "running".into();
            delivery.lease_owner = Some(lease_token);
            delivery.lease_until = Some(lease_until);
            Ok(Some(delivery))
        })
    }

    /// Mark a hook delivery completed (removes row on success).
    /// Fenced to the unique lease token so a stale worker cannot delete a stolen/reclaimed job.
    pub fn complete_hook_delivery(
        &self,
        subscription_key: &str,
        event_id: &str,
        lease_token: &str,
    ) -> Result<bool, OperationError> {
        let deleted = self.execute(
                    "DELETE FROM hook_delivery WHERE subscription_key = ?1 AND event_id = ?2 AND lease_owner = ?3",
                    params![subscription_key, event_id, lease_token],
                )?;
        Ok(deleted > 0)
    }

    /// Handle hook delivery failure with bounded exponential retry or mark exhausted.
    /// Fenced to the unique lease token.
    pub fn fail_hook_delivery(
        &self,
        subscription_key: &str,
        event_id: &str,
        lease_token: &str,
        retry_attempts: u32,
        retry_delay_ms: u64,
    ) -> Result<bool, OperationError> {
        self.transaction(|conn| {
            let now = now_ms();
            let attempts: Option<u32> = conn
                .query_row(
                    "SELECT attempts FROM hook_delivery WHERE subscription_key = ?1 AND event_id = ?2 AND lease_owner = ?3",
                    params![subscription_key, event_id, lease_token],
                    |r| r.get(0),
                )
                .optional()
                ?;

            let Some(attempts) = attempts else {
                return Ok(false);
            };
            let state = if attempts >= retry_attempts {
                "exhausted"
            } else {
                "pending"
            };
            let delay = retry_delay_ms
                .saturating_mul(1u64 << attempts.saturating_sub(1).min(6))
                .min(300_000);
            conn.execute(
                "UPDATE hook_delivery SET state=?4,next_attempt=CASE WHEN ?4='pending' THEN ?5 ELSE next_attempt END,lease_owner=NULL,lease_until=NULL WHERE subscription_key=?1 AND event_id=?2 AND lease_owner=?3",
                params![subscription_key, event_id, lease_token, state, now.saturating_add(delay as i64)],
            )?;
            Ok(true)
        })
    }

    /// Releases a running delivery back to pending (e.g. on cancellation / scope retirement).
    /// Fenced to the unique lease token.
    pub fn release_hook_delivery(
        &self,
        subscription_key: &str,
        event_id: &str,
        lease_token: &str,
    ) -> Result<bool, OperationError> {
        let updated = self.execute(
                    "UPDATE hook_delivery
                     SET state = 'pending',
                         lease_owner = NULL,
                         lease_until = NULL
                     WHERE subscription_key = ?1 AND event_id = ?2 AND lease_owner = ?3 AND state = 'running'",
                    params![subscription_key, event_id, lease_token],
                )?;
        Ok(updated > 0)
    }

    /// Finds the earliest timestamp when a pending record is due or a running lease expires.
    pub fn next_hook_delivery_at(
        &self,
        subscription_key: &str,
    ) -> Result<Option<i64>, OperationError> {
        self.with_read(|c| {
            let now = now_ms();
            c.query_row(
                "SELECT MIN(CASE state WHEN 'pending' THEN next_attempt WHEN 'running' THEN lease_until END) FROM hook_delivery WHERE subscription_key=?1 AND expires>?2",
                params![subscription_key, now], |row| row.get(0),
            ).map_err(OperationError::from)
        })
    }

    /// Metadata-only inspection for debugging (no event payload secrets). Bounded list.
    pub fn hook_deliveries(&self, directory: &str) -> Result<Vec<Value>, OperationError> {
        self.with_read(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT subscription_key, event_id, directory, event_bytes, attempts, next_attempt, expires, state, lease_owner, lease_until
                     FROM hook_delivery
                     WHERE directory = ?1
                     ORDER BY next_attempt DESC
                     LIMIT 256",
                )
                ?;

            let rows = stmt
                .query_map(params![directory], |r| {
                    Ok(json!({
                        "subscriptionKey": r.get::<_, String>(0)?,
                        "eventId": r.get::<_, String>(1)?,
                        "directory": r.get::<_, String>(2)?,
                        "eventBytes": r.get::<_, i64>(3)?,
                        "attempts": r.get::<_, u32>(4)?,
                        "nextAttempt": r.get::<_, i64>(5)?,
                        "expires": r.get::<_, i64>(6)?,
                        "state": r.get::<_, String>(7)?,
                        "leaseOwner": r.get::<_, Option<String>>(8)?,
                        "leaseUntil": r.get::<_, Option<i64>>(9)?,
                    }))
                })
                ?;

            let mut result = Vec::new();
            for r in rows {
                result.push(r?);
            }
            Ok(result)
        })
    }
}
