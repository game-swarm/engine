use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Debug, Default)]
pub struct EngineMetrics {
    healthy: AtomicBool,
    redb_ready: AtomicBool,
    nats_ready: AtomicBool,
    authoritative_tick: AtomicU64,
}

impl EngineMetrics {
    pub fn set_health(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Relaxed);
    }

    pub fn set_dependencies(&self, redb_ready: bool, nats_ready: bool) {
        self.redb_ready.store(redb_ready, Ordering::Relaxed);
        self.nats_ready.store(nats_ready, Ordering::Relaxed);
        self.set_health(redb_ready && nats_ready);
    }

    pub fn set_authoritative_tick(&self, tick: u64) {
        self.authoritative_tick.store(tick, Ordering::Release);
    }

    pub fn authoritative_tick(&self) -> u64 {
        self.authoritative_tick.load(Ordering::Acquire)
    }

    pub fn render(&self) -> String {
        let healthy = bool_sample(self.healthy.load(Ordering::Relaxed));
        let redb_ready = bool_sample(self.redb_ready.load(Ordering::Relaxed));
        let nats_ready = bool_sample(self.nats_ready.load(Ordering::Relaxed));
        let authoritative_tick = self.authoritative_tick.load(Ordering::Acquire);

        format!(
            concat!(
                "# HELP swarm_engine_up Engine process health state from /healthz.\n",
                "# TYPE swarm_engine_up gauge\n",
                "swarm_engine_up {}\n",
                "# HELP swarm_engine_authoritative_tick Current authoritative engine tick.\n",
                "# TYPE swarm_engine_authoritative_tick gauge\n",
                "swarm_engine_authoritative_tick {}\n",
                "# HELP swarm_engine_redb_ready redb dependency readiness.\n",
                "# TYPE swarm_engine_redb_ready gauge\n",
                "swarm_engine_redb_ready {}\n",
                "# HELP swarm_engine_nats_ready NATS dependency readiness.\n",
                "# TYPE swarm_engine_nats_ready gauge\n",
                "swarm_engine_nats_ready {}\n"
            ),
            healthy, authoritative_tick, redb_ready, nats_ready
        )
    }
}

fn bool_sample(value: bool) -> u8 {
    if value { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_ready_and_degraded_samples() {
        let metrics = EngineMetrics::default();
        metrics.set_authoritative_tick(42);
        metrics.set_dependencies(true, false);

        let body = metrics.render();

        assert!(body.contains("# TYPE swarm_engine_up gauge\n"));
        assert!(body.contains("swarm_engine_up 0\n"));
        assert!(body.contains("swarm_engine_authoritative_tick 42\n"));
        assert!(body.contains("swarm_engine_redb_ready 1\n"));
        assert!(body.contains("swarm_engine_nats_ready 0\n"));
        assert!(body.len() < 2048);
    }
}
