import { type KeyboardEventHandler, type RefObject } from "react";
export type UseCommandPaletteOptions<Item> = {
    readonly open: boolean;
    readonly items: readonly Item[];
    readonly filterItems: (items: readonly Item[], query: string) => readonly Item[];
    readonly onOpenChange: (open: boolean) => void;
    readonly onSelect: (item: Item) => void | Promise<void>;
    readonly onEmptyEnter?: ((query: string) => void | Promise<void>) | undefined;
};
export type CommandPaletteState<Item> = {
    readonly inputRef: RefObject<HTMLInputElement | null>;
    readonly query: string;
    readonly setQuery: (query: string) => void;
    readonly filteredItems: readonly Item[];
    readonly selectedIndex: number;
    readonly setSelectedIndex: (index: number) => void;
    readonly activeItem: Item | undefined;
    readonly activate: (item: Item) => void;
    readonly onInputKeyDown: KeyboardEventHandler<HTMLInputElement>;
};
export type UseCommandPaletteShortcutOptions = {
    readonly open: boolean;
    readonly onOpenChange: (open: boolean) => void;
    /** Optional product event that requests opening the palette. */
    readonly openEventName?: string;
    readonly enabled?: boolean;
};
/** Owns the cross-platform Command/Ctrl+K shortcut and optional product event. */
export declare function useCommandPaletteShortcut({ enabled, open, openEventName, onOpenChange, }: UseCommandPaletteShortcutOptions): void;
/**
 * Product-neutral command collection state. Consumers own command creation,
 * filtering vocabulary, rendering, execution, status copy, and routing.
 */
export declare function useCommandPalette<Item>({ open, items, filterItems, onOpenChange, onSelect, onEmptyEnter, }: UseCommandPaletteOptions<Item>): CommandPaletteState<Item>;
//# sourceMappingURL=use-command-palette.d.ts.map