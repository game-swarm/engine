import type { CommandIntent } from "./commands.js";
import type { ValidationResult, WorldSnapshot } from "./types_template.js";
import type { Tick } from "./commands.js";
export type TickPhase = "IDLE" | "COLLECT" | "EXECUTE" | "BROADCAST";
export interface TickProtocolConfig {
    tick_interval_ms: number;
    collect_timeout_ms: number;
    execute_timeout_ms: number;
}
export declare const defaultTickProtocolConfig: TickProtocolConfig;
export interface TickHandler {
    (snapshot: WorldSnapshot): CommandIntent[] | Promise<CommandIntent[]>;
}
export declare function runTick(handler: TickHandler, snapshot: WorldSnapshot): Promise<Uint8Array>;
export declare function parseCommandsFromTickOutput(output: Uint8Array): ValidationResult<CommandIntent[]>;
export declare function makeSnapshotForAi(snapshot: WorldSnapshot): string;
export declare function nextTick(current: Tick): Tick;
//# sourceMappingURL=tick.d.ts.map