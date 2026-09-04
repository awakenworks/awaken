/** The product supplies a fully namespaced key, preventing cross-product collisions. */
export declare function useChatDraft(key: string): {
    readonly value: string;
    readonly setValue: (next: string) => void;
    readonly clear: () => void;
};
