export type GuardedAction = (action: () => void) => boolean;
/**
 * Protects dirty browser state and returns one confirmation gate for in-app
 * dismissal/navigation actions. Products own the localized confirmation copy
 * and any router-specific blocker integration.
 */
export declare function useUnsavedChangesGuard(dirty: boolean, message: string): GuardedAction;
//# sourceMappingURL=use-unsaved-changes-guard.d.ts.map