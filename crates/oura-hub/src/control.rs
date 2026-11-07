//! Server side of the Pi-initiated control channel.
//!
//! The hub queues typed, versioned commands and tracks their lifecycle. The
//! collector opens the outbound WebSocket; the hub never dials the Pi. Commands
//! are allowlisted by `CommandName`; arbitrary names cannot be queued.
//!
//! Expired interactive commands never execute after reconnect. Exactly-once
//! physical execution is not claimed: uncertain outcomes are reported as
//! uncertain and never replayed automatically.

use oura_protocol_v1::{
    CommandName, ControlMessage, ReceiptState, ResultState, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Terminal and non-terminal states a queued command can hold.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Expired,
}

impl CommandState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CommandState::Completed
                | CommandState::Failed
                | CommandState::Cancelled
                | CommandState::Expired
        )
    }
}

/// A command the hub has accepted for delivery.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedCommand {
    pub id: String,
    pub name: CommandName,
    #[serde(default)]
    pub args: serde_json::Value,
    pub created_unix: i64,
    pub expires_unix: i64,
    pub state: CommandState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_detail: Option<serde_json::Value>,
}

impl QueuedCommand {
    pub fn command_frame(&self) -> ControlMessage {
        ControlMessage::Command {
            protocol_version: PROTOCOL_VERSION,
            id: self.id.clone(),
            name: self.name,
            args: self.args.clone(),
            created_unix: self.created_unix,
            expires_unix: self.expires_unix,
        }
    }
}

/// In-memory queue. The hub is single-writer per collector; persistence of the
/// queue itself is intentionally omitted for v1 because a restarted hub cannot
/// prove whether the Pi already received a queued command. On restart the queue
/// is empty and no command silently executes without an operator action.
pub struct ControlQueue {
    commands: BTreeMap<String, QueuedCommand>,
    order: VecDeque<String>,
}

impl Default for ControlQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlQueue {
    pub fn new() -> Self {
        Self {
            commands: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Enqueue an allowlisted command. Rejects an already-used id and an expiry
    /// at or before `now`.
    pub fn enqueue(
        &mut self,
        id: String,
        name: CommandName,
        args: serde_json::Value,
        now: i64,
        ttl: Duration,
    ) -> Result<QueuedCommand, String> {
        if self.commands.contains_key(&id) {
            return Err("duplicate command id".into());
        }
        let ttl_secs = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let expires_unix = now.saturating_add(ttl_secs);
        if expires_unix <= now {
            return Err("command expiry is not in the future".into());
        }
        let command = QueuedCommand {
            id: id.clone(),
            name,
            args,
            created_unix: now,
            expires_unix,
            state: CommandState::Queued,
            receipt_reason: None,
            result_detail: None,
        };
        self.commands.insert(id.clone(), command.clone());
        self.order.push_back(id);
        Ok(command)
    }

    /// Pop the next command that is still valid. Commands whose expiry has passed
    /// are marked `Expired` and removed from the delivery order without being
    /// returned to the collector.
    pub fn next_valid(&mut self, now: i64) -> Option<QueuedCommand> {
        loop {
            let id = self.order.pop_front()?;
            let command = self.commands.get(&id)?;
            if command.state.is_terminal() {
                continue;
            }
            if command.expires_unix <= now {
                if let Some(command) = self.commands.get_mut(&id) {
                    command.state = CommandState::Expired;
                }
                continue;
            }
            if command.state == CommandState::Queued {
                if let Some(command) = self.commands.get_mut(&id) {
                    command.state = CommandState::Running;
                }
                return self.commands.get(&id).cloned();
            }
            // A command already running is not re-delivered; it is either awaiting
            // a result or its outcome is uncertain. Never replay automatically.
        }
    }

    /// Record the collector's receipt for a command. A rejected command becomes
    /// terminal; an accepted one stays running until its result arrives.
    pub fn record_receipt(
        &mut self,
        id: &str,
        state: ReceiptState,
        reason: Option<String>,
    ) -> Option<ControlMessage> {
        let command = self.commands.get_mut(id)?;
        if command.state.is_terminal() {
            return None;
        }
        command.receipt_reason = reason.clone();
        if state == ReceiptState::Rejected {
            command.state = CommandState::Failed;
        }
        Some(ControlMessage::Receipt {
            protocol_version: PROTOCOL_VERSION,
            id: id.to_owned(),
            state,
            reason,
        })
    }

    /// Record a terminal result. An expired command cannot be completed later.
    pub fn record_result(
        &mut self,
        id: &str,
        state: ResultState,
        detail: serde_json::Value,
    ) -> Option<ControlMessage> {
        let command = self.commands.get_mut(id)?;
        if command.state.is_terminal() {
            return None;
        }
        command.state = match state {
            ResultState::Completed => CommandState::Completed,
            ResultState::Failed => CommandState::Failed,
            ResultState::Cancelled => CommandState::Cancelled,
            ResultState::Expired => CommandState::Expired,
        };
        command.result_detail = Some(detail.clone());
        Some(ControlMessage::CommandResult {
            protocol_version: PROTOCOL_VERSION,
            id: id.to_owned(),
            state,
            detail,
        })
    }

    /// Drop commands past their expiry that have not reached a terminal state.
    /// Returns the ids that were expired by this sweep.
    pub fn sweep_expired(&mut self, now: i64) -> Vec<String> {
        let mut expired = Vec::new();
        for (id, command) in &mut self.commands {
            if !command.state.is_terminal() && command.expires_unix <= now {
                command.state = CommandState::Expired;
                expired.push(id.clone());
            }
        }
        self.order.retain(|id| {
            self.commands
                .get(id)
                .is_some_and(|command| !command.state.is_terminal())
        });
        expired
    }

    /// All retained commands, for status reporting and tests.
    ///
    /// Delivered commands leave `order` (so they are never delivered twice) but
    /// remain in `commands` until pruned. Iterating the map keeps them visible
    /// after a receipt or result arrives.
    pub fn snapshot(&self) -> Vec<QueuedCommand> {
        self.commands.values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<&QueuedCommand> {
        self.commands.get(id)
    }

    /// Bounded retention: drop terminal commands older than `retention`.
    pub fn prune(&mut self, now: i64, retention: Duration) {
        let retention_secs = i64::try_from(retention.as_secs()).unwrap_or(i64::MAX);
        let cutoff = now.saturating_sub(retention_secs);
        self.commands
            .retain(|_, command| !(command.state.is_terminal() && command.created_unix < cutoff));
        self.order.retain(|id| self.commands.contains_key(id));
    }
}

/// One event in the browser-facing control stream. Queue snapshots and live HR
/// samples share the channel so a dashboard opens one SSE connection and sees
/// both command lifecycle and bounded live samples.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ControlEvent {
    Queue(Vec<QueuedCommand>),
    Stream(ControlMessage),
}

/// A deadline helper used by the HTTP layer to bound how long a request waits for
/// a collector to appear before returning "no collector connected".
pub struct WaitBudget {
    deadline: Instant,
}

impl WaitBudget {
    pub fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
        }
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.deadline.checked_duration_since(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> i64 {
        1_794_045_440
    }

    #[test]
    fn allowlisted_command_is_queued_and_delivered_once() {
        let mut queue = ControlQueue::new();
        queue
            .enqueue(
                "01J".into(),
                CommandName::SyncNow,
                json!({}),
                now(),
                Duration::from_secs(60),
            )
            .unwrap();
        let delivered = queue.next_valid(now() + 1).unwrap();
        assert_eq!(delivered.id, "01J");
        assert_eq!(delivered.state, CommandState::Running);
        // A running command is not re-delivered: outcome may already be uncertain.
        assert!(queue.next_valid(now() + 2).is_none());
    }

    #[test]
    fn expired_command_never_reaches_the_collector() {
        let mut queue = ControlQueue::new();
        queue
            .enqueue(
                "01J".into(),
                CommandName::SyncNow,
                json!({}),
                now(),
                Duration::from_secs(10),
            )
            .unwrap();
        assert!(queue.next_valid(now() + 11).is_none());
        assert_eq!(queue.get("01J").unwrap().state, CommandState::Expired);
    }

    #[test]
    fn rejected_receipt_is_terminal_and_blocks_result() {
        let mut queue = ControlQueue::new();
        queue
            .enqueue(
                "01J".into(),
                CommandName::SyncNow,
                json!({}),
                now(),
                Duration::from_secs(60),
            )
            .unwrap();
        queue.next_valid(now() + 1).unwrap();
        let receipt = queue
            .record_receipt("01J", ReceiptState::Rejected, Some("busy".into()))
            .unwrap();
        assert!(matches!(
            receipt,
            ControlMessage::Receipt {
                state: ReceiptState::Rejected,
                ..
            }
        ));
        assert!(queue
            .record_result("01J", ResultState::Completed, json!({}))
            .is_none());
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let mut queue = ControlQueue::new();
        queue
            .enqueue(
                "01J".into(),
                CommandName::SyncNow,
                json!({}),
                now(),
                Duration::from_secs(60),
            )
            .unwrap();
        assert!(queue
            .enqueue(
                "01J".into(),
                CommandName::SyncNow,
                json!({}),
                now(),
                Duration::from_secs(60),
            )
            .is_err());
    }

    #[test]
    fn sweep_expires_unfinished_and_prunes_old_terminal() {
        let mut queue = ControlQueue::new();
        queue
            .enqueue(
                "01J".into(),
                CommandName::SyncNow,
                json!({}),
                now(),
                Duration::from_secs(10),
            )
            .unwrap();
        queue
            .enqueue(
                "02J".into(),
                CommandName::CollectorStatus,
                json!({}),
                now(),
                Duration::from_secs(100),
            )
            .unwrap();
        assert_eq!(queue.sweep_expired(now() + 11), vec!["01J".to_owned()]);
        assert!(queue.get("01J").unwrap().state.is_terminal());
        assert_eq!(queue.get("02J").unwrap().state, CommandState::Queued);
    }
}
