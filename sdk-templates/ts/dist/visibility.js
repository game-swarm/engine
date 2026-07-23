export function distance(a, b) {
    if (a.room !== b.room)
        return Number.POSITIVE_INFINITY;
    return Math.max(Math.abs(a.x - b.x), Math.abs(a.y - b.y), Math.abs(a.x + a.y - b.x - b.y));
}
export function visionRange(entity) {
    if (typeof entity.vision_range === "number")
        return entity.vision_range;
    if (entity.type === "drone")
        return 3;
    if (entity.type === "structure" && "structure_type" in entity) {
        if (entity.structure_type === "Observer")
            return 10;
        if (entity.structure_type === "Tower")
            return 3;
        if (entity.structure_type === "Spawn")
            return 3;
    }
    if (entity.type === "controller" && entity.owner)
        return 1;
    return 0;
}
export function isVisibleTo(entity, context) {
    if (context.fog_of_war === false)
        return true;
    if (entity.owner === context.player_id)
        return true;
    return context.entities.some((source) => source.owner === context.player_id && visionRange(source) > 0 && distance(source.position, entity.position) <= visionRange(source));
}
export function visibleEntities(entities, player_id, tick, fog_of_war = true) {
    const context = { player_id, tick, entities, fog_of_war };
    return entities.filter((entity) => isVisibleTo(entity, context));
}
export function canPublicSpectate(config) {
    if (!config.visibility.public_spectate)
        return false;
    if (config.world.mode !== "arena" && config.visibility.spectate_delay < 50)
        return false;
    return config.visibility.replay_privacy !== "private" || config.world.mode === "arena";
}
export function snapshotUsesFogOfWar(config) {
    return config.visibility.fog_of_war;
}
//# sourceMappingURL=visibility.js.map