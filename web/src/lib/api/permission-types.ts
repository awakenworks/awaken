export type PermissionBehavior = "allow" | "ask" | "deny";

export interface PermissionRuleConfig {
  pattern: string;
  behavior: PermissionBehavior;
}

export interface PermissionConfig {
  default_behavior?: PermissionBehavior;
  mode?: string;
  rules?: PermissionRuleConfig[];
}
