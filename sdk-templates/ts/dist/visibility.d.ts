import type { PlayerId, Position } from "./commands.js";
import type { WorldConfig, WorldEntity } from "./types_template.js";
export interface VisibilityContext {
    player_id: PlayerId;
    tick: number | bigint;
    entities: WorldEntity[];
    fog_of_war?: boolean;
}
export declare function distance(a: Position, b: Position): number;
export declare function visionRange(entity: WorldEntity): number;
export declare function isVisibleTo(entity: WorldEntity, context: VisibilityContext): boolean;
export declare function visibleEntities(entities: WorldEntity[], player_id: PlayerId, tick: number | bigint, fog_of_war?: boolean): WorldEntity[];
export declare function canPublicSpectate(config: WorldConfig): boolean;
export declare function snapshotUsesFogOfWar(config: WorldConfig): boolean;
//# sourceMappingURL=visibility.d.ts.map