use bevy::prelude::*;

use crate::systems::LeechResolution;
use crate::tick::{TickTraceEvent, TickTraceEventLog};

pub fn leech_buffer_system(
    mut resolutions: ResMut<LeechResolution>,
    mut trace_events: ResMut<TickTraceEventLog>,
) {
    let mut entries = std::mem::take(&mut resolutions.entries);
    entries.sort_by_key(|entry| {
        (
            entry.target.to_bits(),
            entry.source.to_bits(),
            entry.sort_key,
        )
    });
    trace_events
        .events
        .extend(entries.into_iter().map(|entry| TickTraceEvent {
            system: "leech_buf".to_string(),
            entity: entry.target.to_bits(),
            event: "LeechResolution".to_string(),
            amount: entry.actual_damage,
            resource: Some(format!(
                "source={};self_heal={}",
                entry.source.to_bits(),
                entry.self_heal
            )),
        }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systems::LeechResolutionEntry;

    #[test]
    fn records_leech_resolution_as_bookkeeping_only() {
        let mut app = App::new();
        let source = app.world_mut().spawn_empty().id();
        let target = app.world_mut().spawn_empty().id();
        app.insert_resource(LeechResolution {
            entries: vec![LeechResolutionEntry {
                source,
                target,
                actual_damage: 6,
                self_heal: 3,
                sort_key: 0,
            }],
        });
        app.insert_resource(TickTraceEventLog::default());
        app.add_systems(Update, leech_buffer_system);

        app.update();

        let events = &app.world().resource::<TickTraceEventLog>().events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "LeechResolution");
        assert_eq!(events[0].amount, 6);
    }
}
