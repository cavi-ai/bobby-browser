import type { ControlAction, FormControlTarget, RuntimeCommand } from "./contracts.js";

/**
 * Build a `controlAction` primitive {@link RuntimeCommand}.
 *
 * Requires `target.role` and `target.accessibleName`. Validates `selectMany`
 * uniqueness and non-empty `setFiles` paths.
 */
export function controlActionRuntimeCommand(
  target: FormControlTarget,
  action: ControlAction,
): RuntimeCommand {
  if (!target.role || !target.accessibleName) throw new Error("control target requires role and accessibleName");
  if (action.kind === "selectMany") {
    if (action.values.length === 0 || new Set(action.values).size !== action.values.length) {
      throw new Error("selectMany values must be non-empty and unique");
    }
  }
  if (action.kind === "setFiles" && action.paths.length === 0) {
    throw new Error("setFiles paths must not be empty");
  }
  return { kind: "primitive", input: { kind: "controlAction", input: { target, action } } };
}
