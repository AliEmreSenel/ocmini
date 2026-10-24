use super::*;
use serde::{Deserialize, Serialize};

pub struct HookDeliveryBudget {
    pub retention_ms: u64,
    pub count: usize,
    pub bytes: usize,
    pub total_count: usize,
    pub total_bytes: usize,
}
type DeliveryRow = (String, String, String, i64, u32, i64, i64, String);

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
    /// Inserts a durable hook delivery within an existing domain transaction.
    /// Returns Ok(true) if inserted or already present, Ok(false) if rejected due to outbox quota.
    pub fn persist_hook_delivery_tx(
        tx: &Transaction<'_>,
        subscription_key: &str,
        event_id: &str,
        directory: &str,
        event_json: &str,
        budget: &HookDeliveryBudget,
    ) -> Result<bool, String> {
        let now = now_ms();
        // Expiry is global so renamed/disabled registrations cannot leave
        // abandoned namespaces consuming disk indefinitely.
        tx.execute(
            "DELETE FROM hook_delivery WHERE expires <= ?1",
            params![now],
        )
        .map_err(sql_error)?;

        // Deduplication: check if (subscription_key, event_id) already exists
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM hook_delivery WHERE subscription_key = ?1 AND event_id = ?2)",
                params![subscription_key, event_id],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
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
            .map_err(sql_error)?;

        let event_bytes = event_json.len();
        let (total_count, total_bytes): (i64, i64) = tx
            .query_row(
                "SELECT COUNT(*),COALESCE(SUM(event_bytes),0) FROM hook_delivery",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql_error)?;
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
        .map_err(sql_error)?;

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
    ) -> Result<Option<HookDeliveryRecord>, String> {
        crate::blocking::run(|| {
            let mut conn = self.conn.lock().unwrap();
            let tx = self.write_tx(&mut conn)?;
            let now = now_ms();
            let lease_until = now.saturating_add(lease_ms);

            // Clean expired records
            tx.execute(
                "DELETE FROM hook_delivery WHERE expires <= ?1",
                params![now],
            )
            .map_err(sql_error)?;

            let candidate: Option<DeliveryRow> = tx
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
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                        ))
                    },
                )
                .optional()
                .map_err(sql_error)?;

            let Some((
                event_id,
                directory,
                event_json,
                event_bytes,
                attempts,
                next_attempt,
                expires,
                _state,
            )) = candidate
            else {
                tx.commit().map_err(sql_error)?;
                return Ok(None);
            };

            if attempts >= max_attempts {
                tx.execute(
                    "UPDATE hook_delivery
                     SET attempts = ?1,
                         state = 'exhausted',
                         lease_owner = NULL,
                         lease_until = NULL
                     WHERE subscription_key = ?2 AND event_id = ?3",
                    params![attempts, subscription_key, event_id],
                )
                .map_err(sql_error)?;
                tx.commit().map_err(sql_error)?;
                return Ok(None);
            }

            let new_attempts = attempts.saturating_add(1);

            let lease_token = format!("{}:{}", self.instance, crate::types::hex_id("lst_"));

            tx.execute(
                "UPDATE hook_delivery
                 SET state = 'running',
                     attempts = ?1,
                     lease_owner = ?2,
                     lease_until = ?3
                 WHERE subscription_key = ?4 AND event_id = ?5",
                params![
                    new_attempts,
                    lease_token,
                    lease_until,
                    subscription_key,
                    event_id
                ],
            )
            .map_err(sql_error)?;

            tx.commit().map_err(sql_error)?;

            Ok(Some(HookDeliveryRecord {
                subscription_key: subscription_key.to_string(),
                event_id,
                directory,
                event_json,
                event_bytes: event_bytes as usize,
                attempts: new_attempts,
                next_attempt,
                expires,
                state: "running".into(),
                lease_owner: Some(lease_token),
                lease_until: Some(lease_until),
            }))
        })
    }

    /// Mark a hook delivery completed (removes row on success).
    /// Fenced to the unique lease token so a stale worker cannot delete a stolen/reclaimed job.
    pub fn complete_hook_delivery(
        &self,
        subscription_key: &str,
        event_id: &str,
        lease_token: &str,
    ) -> Result<bool, String> {
        crate::blocking::run(|| {
            let conn = self.conn.lock().unwrap();
            let deleted = conn
                .execute(
                    "DELETE FROM hook_delivery WHERE subscription_key = ?1 AND event_id = ?2 AND lease_owner = ?3",
                    params![subscription_key, event_id, lease_token],
                )
                .map_err(sql_error)?;
            Ok(deleted > 0)
        })
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
    ) -> Result<bool, String> {
        crate::blocking::run(|| {
            let conn = self.conn.lock().unwrap();
            let now = now_ms();
            let attempts: Option<u32> = conn
                .query_row(
                    "SELECT attempts FROM hook_delivery WHERE subscription_key = ?1 AND event_id = ?2 AND lease_owner = ?3",
                    params![subscription_key, event_id, lease_token],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_error)?;

            let attempts = match attempts {
                Some(a) => a,
                None => return Ok(false),
            };

            if attempts >= retry_attempts {
                conn.execute(
                    "UPDATE hook_delivery
                     SET state = 'exhausted',
                         lease_owner = NULL,
                         lease_until = NULL
                     WHERE subscription_key = ?1 AND event_id = ?2 AND lease_owner = ?3",
                    params![subscription_key, event_id, lease_token],
                )
                .map_err(sql_error)?;
            } else {
                let shift = (attempts.saturating_sub(1)).min(6);
                let multiplier = 1u64 << shift;
                let delay = (retry_delay_ms.saturating_mul(multiplier)).min(300_000);
                let next_attempt = now.saturating_add(delay as i64);

                conn.execute(
                    "UPDATE hook_delivery
                     SET next_attempt = ?1,
                         state = 'pending',
                         lease_owner = NULL,
                         lease_until = NULL
                     WHERE subscription_key = ?2 AND event_id = ?3 AND lease_owner = ?4",
                    params![next_attempt, subscription_key, event_id, lease_token],
                )
                .map_err(sql_error)?;
            }
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
    ) -> Result<bool, String> {
        crate::blocking::run(|| {
            let conn = self.conn.lock().unwrap();
            let updated = conn
                .execute(
                    "UPDATE hook_delivery
                     SET state = 'pending',
                         lease_owner = NULL,
                         lease_until = NULL
                     WHERE subscription_key = ?1 AND event_id = ?2 AND lease_owner = ?3 AND state = 'running'",
                    params![subscription_key, event_id, lease_token],
                )
                .map_err(sql_error)?;
            Ok(updated > 0)
        })
    }

    /// Finds the earliest timestamp when a pending record is due or a running lease expires.
    pub fn next_hook_delivery_at(&self, subscription_key: &str) -> Result<Option<i64>, String> {
        self.with_read(|c| {
            let now = now_ms();
            let row: Option<(Option<i64>, Option<i64>)> = c
                .query_row(
                    "SELECT
                        (SELECT MIN(next_attempt) FROM hook_delivery WHERE subscription_key = ?1 AND state = 'pending' AND expires > ?2),
                        (SELECT MIN(lease_until) FROM hook_delivery WHERE subscription_key = ?1 AND state = 'running' AND expires > ?2)",
                    params![subscription_key, now],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sql_error)?;

            let Some((min_pending, min_lease)) = row else {
                return Ok(None);
            };

            let earliest = match (min_pending, min_lease) {
                (Some(p), Some(l)) => Some(p.min(l)),
                (Some(p), None) => Some(p),
                (None, Some(l)) => Some(l),
                (None, None) => None,
            };

            Ok(earliest)
        })
    }

    /// Metadata-only inspection for debugging (no event payload secrets). Bounded list.
    pub fn hook_deliveries(&self, directory: &str) -> Result<Vec<Value>, String> {
        self.with_read(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT subscription_key, event_id, directory, event_bytes, attempts, next_attempt, expires, state, lease_owner, lease_until
                     FROM hook_delivery
                     WHERE directory = ?1
                     ORDER BY next_attempt DESC
                     LIMIT 256",
                )
                .map_err(sql_error)?;

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
                .map_err(sql_error)?;

            let mut result = Vec::new();
            for r in rows {
                result.push(r.map_err(sql_error)?);
            }
            Ok(result)
        })
    }
}
