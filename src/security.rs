#[derive(Debug, Clone)]
pub struct SecurityAuditor {
    pub enabled: bool,
    /// Maximum fuel consumption per tick that doesn't trigger an alert
    pub max_fuel_per_tick: u64,
    /// Maximum consecutive rejected commands before flagging
    pub max_consecutive_rejects: u32,
}

impl Default for SecurityAuditor {
    fn default() -> Self {
        Self {
            enabled: true,
            max_fuel_per_tick: 1_000_000,
            max_consecutive_rejects: 50,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    Anomaly {
        player_id: PlayerId,
        tick: u64,
        description: String,
    },
}

use crate::components::PlayerId;

#[cfg(test)]
mod tests {
    use super::*;

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
}
