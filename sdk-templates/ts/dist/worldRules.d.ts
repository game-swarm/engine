import type { BodyPart, ResourceCost } from "./commands.js";
import type { ValidationIssue, WorldConfig, WorldMode } from "./types_template.js";
export declare function createDefaultWorldConfig(mode?: WorldMode): WorldConfig;
export declare function validateWorldConfig(config: WorldConfig): ValidationIssue[];
export declare function actionCost(config: WorldConfig, action: string, detail?: string): ResourceCost;
export declare function bodyPartCost(config: WorldConfig, part: BodyPart): ResourceCost;
export declare function bodyCostFromRules(config: WorldConfig, body: BodyPart[]): ResourceCost;
//# sourceMappingURL=worldRules.d.ts.map