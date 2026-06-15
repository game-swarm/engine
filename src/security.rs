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
            security_alerts: Vec::new(),
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
}
