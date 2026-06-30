use bevy::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Resource, Debug, Clone, Default, Serialize, Deserialize)]
pub struct BossTemplate {
    pub name: String,
    pub mode: BossMode,
    pub hits: u32,
    pub phases: Vec<u32>,
    pub drops: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BossMode {
    #[default]
    World,
    Arena,
}

#[derive(Resource, Debug, Clone)]
pub struct VanillaBossConfig {
    pub boss_templates: Vec<BossTemplate>,
    pub arena_bosses_enabled: bool,
    pub world_bosses_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct VanillaBossPlugin {
    pub boss_templates: Vec<BossTemplate>,
    pub arena_bosses_enabled: bool,
    pub world_bosses_enabled: bool,
}

impl Default for VanillaBossPlugin {
    fn default() -> Self {
        Self {
            boss_templates: vec![
                BossTemplate {
                    name: "world-alpha".to_string(),
                    mode: BossMode::World,
                    hits: 100_000,
                    phases: vec![75, 50, 25],
                    drops: vec!["Energy".to_string(), "Mineral".to_string()],
                },
                BossTemplate {
                    name: "arena-champion".to_string(),
                    mode: BossMode::Arena,
                    hits: 50_000,
                    phases: vec![50],
                    drops: vec!["ArenaToken".to_string()],
                },
            ],
            arena_bosses_enabled: true,
            world_bosses_enabled: false,
        }
    }
}

impl Plugin for VanillaBossPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(VanillaBossConfig {
            boss_templates: self.boss_templates.clone(),
            arena_bosses_enabled: self.arena_bosses_enabled,
            world_bosses_enabled: self.world_bosses_enabled,
        });
        app.add_systems(
            Update,
            (
                boss_spawn_system,
                boss_phase_trigger_system,
                boss_drop_system,
            )
                .chain(),
        );
    }
}

pub fn boss_spawn_system() {}

pub fn boss_phase_trigger_system() {}

pub fn boss_drop_system() {}
