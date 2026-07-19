use bevy::prelude::*;
use indexmap::IndexMap;
use swarm_engine_plugin_sdk::components::Drone;

use crate::command::{ObjectId, entity, object_id};
use crate::components::DroneEnv;
use crate::world::WorldConfig;

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct DroneEnvVars(pub IndexMap<ObjectId, IndexMap<String, Option<String>>>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DroneEnvVarError {
    Disabled,
    ObjectNotFound,
    NotDrone,
}

pub fn drone_env_var_system(
    config: Res<WorldConfig>,
    mut pending: ResMut<DroneEnvVars>,
    mut drones: Query<(Entity, &mut DroneEnv), With<Drone>>,
) {
    if !config.drone.env_vars {
        pending.0.clear();
        return;
    }

    let updates = std::mem::take(&mut pending.0);
    for (entity, mut env) in drones.iter_mut() {
        let Some(changes) = updates.get(&object_id(entity)) else {
            continue;
        };
        for (key, value) in changes {
            match value {
                Some(value) => {
                    env.vars.insert(key.clone(), value.clone());
                }
                None => {
                    env.vars.shift_remove(key);
                }
            }
        }
        trim_to_memory_size(&mut env.vars, config.drone.memory_size as usize);
    }
}

pub fn read_drone_env_var(
    world: &World,
    drone_id: ObjectId,
    key: &str,
) -> Result<Option<String>, DroneEnvVarError> {
    if !world.resource::<WorldConfig>().drone.env_vars {
        return Err(DroneEnvVarError::Disabled);
    }
    let entity = entity(drone_id).map_err(|_| DroneEnvVarError::ObjectNotFound)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| DroneEnvVarError::ObjectNotFound)?;
    if entity_ref.get::<Drone>().is_none() {
        return Err(DroneEnvVarError::NotDrone);
    }
    Ok(entity_ref
        .get::<DroneEnv>()
        .and_then(|env| env.vars.get(key).cloned()))
}

pub fn write_drone_env_var(
    world: &mut World,
    drone_id: ObjectId,
    key: impl Into<String>,
    value: Option<String>,
) -> Result<(), DroneEnvVarError> {
    if !world.resource::<WorldConfig>().drone.env_vars {
        return Err(DroneEnvVarError::Disabled);
    }
    let entity = entity(drone_id).map_err(|_| DroneEnvVarError::ObjectNotFound)?;
    let max_size = world.resource::<WorldConfig>().drone.memory_size as usize;
    let mut entity_mut = world
        .get_entity_mut(entity)
        .map_err(|_| DroneEnvVarError::ObjectNotFound)?;
    if entity_mut.get::<Drone>().is_none() {
        return Err(DroneEnvVarError::NotDrone);
    }
    if entity_mut.get::<DroneEnv>().is_none() {
        entity_mut.insert(DroneEnv::default());
    }
    let mut env = entity_mut.get_mut::<DroneEnv>().unwrap();
    let key = key.into();
    match value {
        Some(value) => {
            env.vars.insert(key, value);
        }
        None => {
            env.vars.shift_remove(&key);
        }
    }
    trim_to_memory_size(&mut env.vars, max_size);
    Ok(())
}

fn trim_to_memory_size(vars: &mut IndexMap<String, String>, max_size: usize) {
    while env_size(vars) > max_size {
        if vars.shift_remove_index(0).is_none() {
            break;
        }
    }
}

fn env_size(vars: &IndexMap<String, String>) -> usize {
    vars.iter()
        .map(|(key, value)| key.len() + value.len())
        .sum()
}
