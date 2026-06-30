use crate::components::PlayerId;
use crate::tick::{ReplayError, TickTrace, replay_tick};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct SecurityAuditor {
    pub enabled: bool,
    /// Maximum fuel consumption per tick that doesn't trigger an alert
    pub max_fuel_per_tick: u64,
    /// Maximum consecutive rejected commands before flagging
    pub max_consecutive_rejects: u32,
    /// Multiplier above recent baseline fuel usage that flags a spike.
    pub fuel_spike_multiplier: u64,
    /// Minimum fuel delta required before multiplier based spike checks apply.
    pub min_fuel_spike_delta: u64,
    /// Rejection-rate delta above recent baseline that flags a spike.
    pub rejection_rate_spike_delta: f64,
}

impl Default for SecurityAuditor {
    fn default() -> Self {
        Self {
            enabled: true,
            max_fuel_per_tick: 1_000_000,
            max_consecutive_rejects: 50,
            fuel_spike_multiplier: 4,
            min_fuel_spike_delta: 100_000,
            rejection_rate_spike_delta: 0.5,
        }
    }
}

impl SecurityAuditor {
    pub fn audit_tick(
        &self,
        player_id: PlayerId,
        fuel_used: u64,
        rejected: u32,
        tick: u64,
    ) -> Vec<SecurityAlert> {
        let mut alerts = Vec::new();
        if !self.enabled {
            return alerts;
        }

        if fuel_used > self.max_fuel_per_tick {
            alerts.push(SecurityAlert::HighFuel {
                player_id,
                tick,
                fuel_used,
                limit: self.max_fuel_per_tick,
            });
        }

        if rejected > self.max_consecutive_rejects {
            alerts.push(SecurityAlert::RejectionSpike {
                player_id,
                tick,
                rejected,
                limit: self.max_consecutive_rejects,
            });
        }

        alerts
    }

    pub fn audit_trace(
        &self,
        trace: &TickTrace,
        previous: Option<&TickTrace>,
    ) -> Vec<SecurityAlert> {
        let mut alerts = self.audit_tick(
            trace.player_id,
            trace.metrics.fuel_consumed,
            trace.metrics.rejected_commands as u32,
            trace.tick,
        );
        if !self.enabled {
            return alerts;
        }

        if let Some(previous) = previous {
            if previous.player_id == trace.player_id {
                let baseline_fuel = previous.metrics.fuel_consumed.max(1);
                let fuel_delta = trace.metrics.fuel_consumed.saturating_sub(baseline_fuel);
                if fuel_delta >= self.min_fuel_spike_delta
                    && trace.metrics.fuel_consumed
                        > baseline_fuel.saturating_mul(self.fuel_spike_multiplier)
                {
                    alerts.push(SecurityAlert::FuelSpike {
                        player_id: trace.player_id,
                        tick: trace.tick,
                        fuel_used: trace.metrics.fuel_consumed,
                        baseline: baseline_fuel,
                    });
                }

                let rejection_rate = trace.metrics.command_rejection_rate();
                let baseline_rate = previous.metrics.command_rejection_rate();
                if trace.metrics.total_commands > 0
                    && rejection_rate >= baseline_rate + self.rejection_rate_spike_delta
                {
                    alerts.push(SecurityAlert::RejectionRateSpike {
                        player_id: trace.player_id,
                        tick: trace.tick,
                        rejection_rate,
                        baseline: baseline_rate,
                    });
                }

                if previous.tick.saturating_add(1) == trace.tick {
                    match replay_tick(&previous.state, trace) {
                        Ok(replayed_state) if replayed_state != trace.state => {
                            alerts.push(SecurityAlert::StateInconsistency {
                                player_id: trace.player_id,
                                tick: trace.tick,
                                expected_checksum: trace.state_checksum,
                                actual_checksum: checksum_state(&replayed_state),
                            });
                        }
                        Err(ReplayError::StateMismatch {
                            expected_checksum,
                            actual_checksum,
                            ..
                        }) => alerts.push(SecurityAlert::StateInconsistency {
                            player_id: trace.player_id,
                            tick: trace.tick,
                            expected_checksum,
                            actual_checksum,
                        }),
                        Err(error) => alerts.push(SecurityAlert::Anomaly {
                            player_id: trace.player_id,
                            tick: trace.tick,
                            description: format!("replay mismatch: {error:?}"),
                        }),
                        Ok(_) => {}
                    }
                }
            }
        }

        alerts
    }

    pub fn audit_wasm_module(
        &self,
        player_id: PlayerId,
        tick: u64,
        wasm: &[u8],
    ) -> Vec<SecurityAlert> {
        if !self.enabled {
            return Vec::new();
        }
        WasmModuleAuditor::default().audit(player_id, tick, wasm)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SecurityAlert {
    HighFuel {
        player_id: PlayerId,
        tick: u64,
        fuel_used: u64,
        limit: u64,
    },
    RejectionSpike {
        player_id: PlayerId,
        tick: u64,
        rejected: u32,
        limit: u32,
    },
    FuelSpike {
        player_id: PlayerId,
        tick: u64,
        fuel_used: u64,
        baseline: u64,
    },
    RejectionRateSpike {
        player_id: PlayerId,
        tick: u64,
        rejection_rate: f64,
        baseline: f64,
    },
    StateInconsistency {
        player_id: PlayerId,
        tick: u64,
        expected_checksum: u64,
        actual_checksum: u64,
    },
    SuspiciousWasm {
        player_id: PlayerId,
        tick: u64,
        pattern: String,
    },
    Anomaly {
        player_id: PlayerId,
        tick: u64,
        description: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ReplayAuditor {
    pub security: SecurityAuditor,
}

impl ReplayAuditor {
    pub fn audit(&self, recorded: &[TickTrace], replayed: &[TickTrace]) -> Vec<SecurityAlert> {
        let mut alerts = Vec::new();
        let len = recorded.len().min(replayed.len());
        for index in 0..len {
            let expected = &recorded[index];
            let actual = &replayed[index];
            if expected.tick != actual.tick
                || expected.player_id != actual.player_id
                || expected.commands != actual.commands
                || expected.rejections != actual.rejections
                || expected.state != actual.state
                || expected.state_checksum != actual.state_checksum
            {
                alerts.push(SecurityAlert::StateInconsistency {
                    player_id: expected.player_id,
                    tick: expected.tick,
                    expected_checksum: expected.state_checksum,
                    actual_checksum: actual.state_checksum,
                });
            }
            alerts.extend(
                self.security
                    .audit_trace(expected, index.checked_sub(1).map(|i| &recorded[i])),
            );
        }

        if recorded.len() != replayed.len() {
            let trace = recorded.last().or_else(|| replayed.last());
            alerts.push(SecurityAlert::Anomaly {
                player_id: trace.map(|trace| trace.player_id).unwrap_or_default(),
                tick: trace.map(|trace| trace.tick).unwrap_or_default(),
                description: format!(
                    "replay length mismatch: recorded={}, replayed={}",
                    recorded.len(),
                    replayed.len()
                ),
            });
        }

        alerts
    }
}

#[derive(Debug, Clone)]
pub struct WasmModuleAuditor {
    suspicious_imports: Vec<&'static [u8]>,
    suspicious_exports: Vec<&'static [u8]>,
}

impl Default for WasmModuleAuditor {
    fn default() -> Self {
        Self {
            suspicious_imports: vec![b"wasi_", b"random_get", b"clock_time_get", b"fd_", b"sock_"],
            suspicious_exports: vec![b"_start", b"__wasm_call_ctors"],
        }
    }
}

impl WasmModuleAuditor {
    pub fn audit(&self, player_id: PlayerId, tick: u64, wasm: &[u8]) -> Vec<SecurityAlert> {
        let mut alerts = Vec::new();
        if !wasm.starts_with(b"\0asm") {
            alerts.push(SecurityAlert::SuspiciousWasm {
                player_id,
                tick,
                pattern: "missing wasm magic".to_string(),
            });
            return alerts;
        }

        if has_start_section(wasm) {
            alerts.push(SecurityAlert::SuspiciousWasm {
                player_id,
                tick,
                pattern: "start section".to_string(),
            });
        }

        for pattern in self
            .suspicious_imports
            .iter()
            .chain(self.suspicious_exports.iter())
        {
            if contains_bytes(wasm, pattern) {
                alerts.push(SecurityAlert::SuspiciousWasm {
                    player_id,
                    tick,
                    pattern: String::from_utf8_lossy(pattern).to_string(),
                });
            }
        }

        alerts
    }
}

fn has_start_section(wasm: &[u8]) -> bool {
    let mut offset = 8;
    while offset < wasm.len() {
        let section_id = wasm[offset];
        offset += 1;
        let Some((section_len, len_bytes)) = read_leb_u32(&wasm[offset..]) else {
            return false;
        };
        offset += len_bytes;
        if section_id == 8 {
            return true;
        }
        offset = offset.saturating_add(section_len as usize);
    }
    false
}

fn read_leb_u32(bytes: &[u8]) -> Option<(u32, usize)> {
    let mut result = 0_u32;
    for (index, byte) in bytes.iter().take(5).enumerate() {
        result |= ((byte & 0x7f) as u32) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((result, index + 1));
        }
    }
    None
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn checksum_state<T: std::fmt::Debug>(state: &T) -> u64 {
    let hash = blake3::hash(format!("{state:?}").as_bytes());
    u64::from_le_bytes(hash.as_bytes()[0..8].try_into().unwrap())
}

// ═══════════════════════════════════════════════════════════════════
// W16c: Session + Deploy State Machines (§7.1-§7.4)
// Spec: docs/specs/security/09-command-source.md
// ═══════════════════════════════════════════════════════════════════

use std::collections::HashMap;

/// Session lifecycle states (§7.1)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Active,
    PendingClose,
    Closed,
}

/// Per-connection session state (§7.1)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: u128,
    pub player_id: PlayerId,
    pub status: SessionStatus,
    pub created_at: u64,
    pub last_heartbeat: u64,
    pub expires_at: u64,
    /// Refund credit accumulated this session (per §7.2, scoped to player+slot+session+tick_window)
    pub refund_credit: u64,
}

impl SessionState {
    pub const RECONNECT_WINDOW_TICKS: u64 = 60;
    pub const HEARTBEAT_INTERVAL_TICKS: u64 = 30;

    pub fn new(session_id: u128, player_id: PlayerId, now: u64) -> Self {
        Self {
            session_id,
            player_id,
            status: SessionStatus::Active,
            created_at: now,
            last_heartbeat: now,
            expires_at: now + Self::RECONNECT_WINDOW_TICKS,
            refund_credit: 0,
        }
    }

    pub fn heartbeat(&mut self, now: u64) {
        self.last_heartbeat = now;
        self.expires_at = now + Self::RECONNECT_WINDOW_TICKS;
    }

    pub fn is_expired(&self, now: u64) -> bool {
        now > self.expires_at
    }

    pub fn can_reconnect(&self, now: u64) -> bool {
        matches!(self.status, SessionStatus::PendingClose) && !self.is_expired(now)
    }
}

/// Per-player, per-slot monotonic version counter (§7.3)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeployVersionCounters {
    /// Key: (player_id, slot_name) → current version_counter
    counters: HashMap<(PlayerId, String), u64>,
}

impl DeployVersionCounters {
    /// Returns true if this version_counter is newer (anti-replay)
    pub fn check_and_advance(&mut self, player_id: PlayerId, slot: &str, counter: u64) -> bool {
        let key = (player_id, slot.to_string());
        let current = self.counters.get(&key).copied().unwrap_or(0);
        if counter > current {
            self.counters.insert(key, counter);
            true
        } else {
            false
        }
    }

    pub fn current(&self, player_id: PlayerId, slot: &str) -> u64 {
        self.counters
            .get(&(player_id, slot.to_string()))
            .copied()
            .unwrap_or(0)
    }
}

/// Deploy nonce state machine (§7.4)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployNonceState {
    Idle,
    Compiling,
    Deployed,
    Rejected,
}

/// Per-deploy nonce tracker (§7.4)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployNonce {
    pub state: DeployNonceState,
    pub player_id: PlayerId,
    pub slot: String,
    pub version_counter: u64,
    pub module_hash: Option<[u8; 32]>,
    pub metadata_hash: Option<[u8; 32]>,
    pub deployed_at: Option<u64>,
}

impl DeployNonce {
    pub fn new(player_id: PlayerId, slot: &str, version_counter: u64) -> Self {
        Self {
            state: DeployNonceState::Idle,
            player_id,
            slot: slot.to_string(),
            version_counter,
            module_hash: None,
            metadata_hash: None,
            deployed_at: None,
        }
    }

    pub fn transition(&mut self, to: DeployNonceState) {
        self.state = to;
        if to == DeployNonceState::Deployed {
            self.deployed_at = Some(0); // tick set by caller
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// W16c: Safe Hint Ladder — 三级错误提示模型
// ═══════════════════════════════════════════════════════════════════

/// Three-level error hint escalation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HintLevel {
    /// Level 1: Direct fix suggestion (e.g. "need 50 more energy")
    Direct,
    /// Level 2: Reference docs/SDK (e.g. "see swarm_get_docs('harvest')")
    Reference,
    /// Level 3: Escalate to human (e.g. "unexpected state, contact admin")
    Escalate,
}

/// Structured hint with escalation level
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeHint {
    pub level: HintLevel,
    pub summary: String,
    pub detail: String,
    pub doc_ref: Option<String>,
}

impl SafeHint {
    pub fn direct(summary: &str, detail: &str) -> Self {
        Self {
            level: HintLevel::Direct,
            summary: summary.to_string(),
            detail: detail.to_string(),
            doc_ref: None,
        }
    }

    pub fn reference(summary: &str, detail: &str, doc_ref: &str) -> Self {
        Self {
            level: HintLevel::Reference,
            summary: summary.to_string(),
            detail: detail.to_string(),
            doc_ref: Some(doc_ref.to_string()),
        }
    }

    pub fn escalate(summary: &str, detail: &str) -> Self {
        Self {
            level: HintLevel::Escalate,
            summary: summary.to_string(),
            detail: detail.to_string(),
            doc_ref: None,
        }
    }
}

/// Ladder that maps rejection reasons to appropriate hint levels
pub struct SafeHintLadder;

impl SafeHintLadder {
    /// Produce the appropriate SafeHint for a rejection reason
    pub fn hint_for(reason: &str) -> SafeHint {
        match reason {
            // Level 1 — Direct fix
            r if r.contains("insufficient") && r.contains("energy") => {
                SafeHint::direct("Need more energy", r)
            }
            r if r.contains("cooldown") => SafeHint::direct("Action on cooldown", r),
            r if r.contains("room is full") => SafeHint::direct("Room at capacity", r),

            // Level 2 — Reference docs
            r if r.contains("body_part") => SafeHint::reference(
                "Invalid body part targeting",
                r,
                "swarm_get_docs('body_parts')",
            ),
            r if r.contains("disrupt") => SafeHint::reference(
                "Disrupt requires valid body part match",
                r,
                "swarm_get_docs('disrupt')",
            ),
            r if r.contains("hack") || r.contains("drain") || r.contains("overload") => {
                SafeHint::reference(
                    "Special attack validation failed",
                    r,
                    "swarm_get_docs('special_attacks')",
                )
            }

            // Level 3 — Escalate
            r if r.contains("state") && r.contains("unexpected") => {
                SafeHint::escalate("Unexpected engine state", r)
            }
            _ => SafeHint::direct("Command rejected", reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::PlayerLocalStorage;
    use crate::tick::{TickMetrics, TickState};
    use crate::{create_world, energy_cost};

    fn trace(tick: u64, player_id: PlayerId, metrics: TickMetrics, local_energy: u32) -> TickTrace {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<PlayerLocalStorage>()
            .0
            .insert(player_id, energy_cost(local_energy));
        let state = TickState::capture(world.app.world_mut());
        TickTrace {
            tick,
            player_id,
            commands: Vec::new(),
            state,
            rejections: Vec::new(),
            metrics,
            state_checksum: world.state_checksum(),
            system_manifest_hash: [0; 32],
            action_manifest_hash: [0; 32],
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
        }
    }

    #[test]
    fn auditor_detects_high_fuel() {
        let auditor = SecurityAuditor::default();
        let alerts = auditor.audit_tick(1u32, 2_000_000, 0, 42);
        assert_eq!(alerts.len(), 1);
        assert!(matches!(alerts[0], SecurityAlert::HighFuel { .. }));
    }

    #[test]
    fn auditor_detects_rejection_spike() {
        let auditor = SecurityAuditor::default();
        let alerts = auditor.audit_tick(1u32, 1000, 100, 42);
        assert_eq!(alerts.len(), 1);
        assert!(matches!(alerts[0], SecurityAlert::RejectionSpike { .. }));
    }

    #[test]
    fn auditor_disabled_returns_empty() {
        let auditor = SecurityAuditor {
            enabled: false,
            ..Default::default()
        };
        let alerts = auditor.audit_tick(1u32, 2_000_000, 100, 42);
        assert!(alerts.is_empty());
    }

    #[test]
    fn normal_trace_and_minimal_wasm_do_not_alert() {
        let auditor = SecurityAuditor::default();
        let previous = trace(
            1,
            7,
            TickMetrics {
                total_commands: 4,
                accepted_commands: 4,
                fuel_consumed: 40_000,
                ..Default::default()
            },
            100,
        );
        let current = trace(
            2,
            7,
            TickMetrics {
                total_commands: 4,
                accepted_commands: 4,
                fuel_consumed: 45_000,
                ..Default::default()
            },
            100,
        );

        assert!(auditor.audit_trace(&current, Some(&previous)).is_empty());
        assert!(
            auditor
                .audit_wasm_module(7, 2, b"\0asm\x01\0\0\0")
                .is_empty()
        );
    }

    #[test]
    fn auditor_detects_fuel_and_rejection_rate_spikes() {
        let auditor = SecurityAuditor::default();
        let previous = trace(
            1,
            7,
            TickMetrics {
                total_commands: 20,
                accepted_commands: 20,
                fuel_consumed: 50_000,
                ..Default::default()
            },
            100,
        );
        let current = trace(
            2,
            7,
            TickMetrics {
                total_commands: 20,
                rejected_commands: 18,
                fuel_consumed: 350_000,
                ..Default::default()
            },
            100,
        );

        let alerts = auditor.audit_trace(&current, Some(&previous));
        assert!(
            alerts
                .iter()
                .any(|alert| matches!(alert, SecurityAlert::FuelSpike { .. }))
        );
        assert!(
            alerts
                .iter()
                .any(|alert| matches!(alert, SecurityAlert::RejectionRateSpike { .. }))
        );
    }

    #[test]
    fn auditor_detects_cross_tick_state_inconsistency() {
        let auditor = SecurityAuditor::default();
        let previous = trace(1, 7, TickMetrics::default(), 100);
        let current = trace(2, 7, TickMetrics::default(), 200);

        let alerts = auditor.audit_trace(&current, Some(&previous));
        assert!(
            alerts
                .iter()
                .any(|alert| matches!(alert, SecurityAlert::StateInconsistency { .. }))
        );
    }

    #[test]
    fn wasm_auditor_detects_suspicious_patterns() {
        let auditor = SecurityAuditor::default();
        let mut suspicious = b"\0asm\x01\0\0\0".to_vec();
        suspicious.extend_from_slice(b"random_get");

        let alerts = auditor.audit_wasm_module(9, 3, &suspicious);
        assert!(alerts.iter().any(|alert| matches!(
            alert,
            SecurityAlert::SuspiciousWasm { pattern, .. } if pattern == "random_get"
        )));

        let invalid = auditor.audit_wasm_module(9, 3, b"not wasm");
        assert!(invalid.iter().any(|alert| matches!(
            alert,
            SecurityAlert::SuspiciousWasm { pattern, .. } if pattern == "missing wasm magic"
        )));
    }

    #[test]
    fn replay_auditor_compares_recorded_and_replayed_traces() {
        let recorded = vec![trace(1, 7, TickMetrics::default(), 100)];
        let replayed = vec![trace(1, 7, TickMetrics::default(), 200)];

        let alerts = ReplayAuditor::default().audit(&recorded, &replayed);
        assert!(
            alerts
                .iter()
                .any(|alert| matches!(alert, SecurityAlert::StateInconsistency { .. }))
        );
    }

    // ── W16c: Session + Deploy State Machines + Safe Hint Ladder ──

    #[test]
    fn session_lifecycle_active_to_closed() {
        let mut session = SessionState::new(1, 42, 100);
        assert_eq!(session.status, SessionStatus::Active);
        assert!(!session.is_expired(100));
        assert!(!session.is_expired(159));
        assert!(session.is_expired(161));
    }

    #[test]
    fn session_heartbeat_extends_lifetime() {
        let mut session = SessionState::new(1, 42, 100);
        session.heartbeat(150);
        assert!(!session.is_expired(150));
        assert!(!session.is_expired(209));
        assert!(session.is_expired(211));
    }

    #[test]
    fn session_reconnect_window() {
        let mut session = SessionState::new(1, 42, 100);
        session.status = SessionStatus::PendingClose;
        assert!(session.can_reconnect(150));
        assert!(!session.can_reconnect(170));
    }

    #[test]
    fn version_counter_anti_replay() {
        let mut counters = DeployVersionCounters::default();
        assert!(counters.check_and_advance(1, "main", 1));
        assert_eq!(counters.current(1, "main"), 1);
        assert!(!counters.check_and_advance(1, "main", 1));
        assert!(!counters.check_and_advance(1, "main", 0));
        assert!(counters.check_and_advance(1, "main", 5));
        assert_eq!(counters.current(1, "main"), 5);
    }

    #[test]
    fn version_counter_per_slot_isolation() {
        let mut counters = DeployVersionCounters::default();
        assert!(counters.check_and_advance(1, "main", 3));
        assert!(counters.check_and_advance(1, "defense", 7));
        assert_eq!(counters.current(1, "main"), 3);
        assert_eq!(counters.current(1, "defense"), 7);
    }

    #[test]
    fn deploy_nonce_state_machine() {
        let mut nonce = DeployNonce::new(42, "main", 1);
        assert_eq!(nonce.state, DeployNonceState::Idle);
        nonce.transition(DeployNonceState::Compiling);
        assert_eq!(nonce.state, DeployNonceState::Compiling);
        nonce.transition(DeployNonceState::Deployed);
        assert_eq!(nonce.state, DeployNonceState::Deployed);
        assert!(nonce.deployed_at.is_some());
    }

    #[test]
    fn safe_hint_level_1_direct_for_insufficient_energy() {
        let hint = SafeHintLadder::hint_for("insufficient energy: have 10, need 50");
        assert_eq!(hint.level, HintLevel::Direct);
        assert!(hint.summary.contains("Need more energy"));
    }

    #[test]
    fn safe_hint_level_2_reference_for_body_part() {
        let hint = SafeHintLadder::hint_for("invalid body_part targeting");
        assert_eq!(hint.level, HintLevel::Reference);
        assert!(hint.doc_ref.unwrap().contains("body_parts"));
    }

    #[test]
    fn safe_hint_level_3_escalate_for_unexpected_state() {
        let hint = SafeHintLadder::hint_for("unexpected state in combat resolver");
        assert_eq!(hint.level, HintLevel::Escalate);
    }

    #[test]
    fn safe_hint_defaults_to_direct() {
        let hint = SafeHintLadder::hint_for("unknown validation error");
        assert_eq!(hint.level, HintLevel::Direct);
    }
}
