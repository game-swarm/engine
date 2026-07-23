import type { CommandIntent, TickInput, TickResult } from "./commands.js";
import type { ValidationIssue, ValidationResult } from "./types_template.js";
export declare function parseTickOutput(input: Uint8Array): ValidationResult<CommandIntent[]>;
export declare function serializeTickOutput(commands: CommandIntent[]): Uint8Array;
export declare function encodeTickInput(input: TickInput): Uint8Array;
export declare function decodeTickInput(input: Uint8Array): ValidationResult<TickInput>;
export declare function encodeTickResult(result: TickResult): Uint8Array;
export declare function decodeTickResult(input: Uint8Array): ValidationResult<TickResult>;
export declare function stringifyJson(value: unknown): string;
export declare function validateCommandIntents(value: unknown): ValidationResult<CommandIntent[]>;
export declare function validateCommandIntent(value: unknown, path?: string, issues?: ValidationIssue[]): ValidationIssue[];
export declare function validateAction(value: unknown, path?: string, issues?: ValidationIssue[]): ValidationIssue[];
export declare function jsonDepth(value: unknown): number;
//# sourceMappingURL=validation.d.ts.map