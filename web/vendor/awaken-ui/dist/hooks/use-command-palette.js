import { useEffect, useMemo, useRef, useState, } from "react";
/** Owns the cross-platform Command/Ctrl+K shortcut and optional product event. */
export function useCommandPaletteShortcut({ enabled = true, open, openEventName, onOpenChange, }) {
    useEffect(() => {
        if (!enabled)
            return;
        const onKeyDown = (event) => {
            if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
                event.preventDefault();
                onOpenChange(!open);
            }
        };
        const onOpen = () => onOpenChange(true);
        window.addEventListener("keydown", onKeyDown);
        if (openEventName)
            window.addEventListener(openEventName, onOpen);
        return () => {
            window.removeEventListener("keydown", onKeyDown);
            if (openEventName)
                window.removeEventListener(openEventName, onOpen);
        };
    }, [enabled, onOpenChange, open, openEventName]);
}
/**
 * Product-neutral command collection state. Consumers own command creation,
 * filtering vocabulary, rendering, execution, status copy, and routing.
 */
export function useCommandPalette({ open, items, filterItems, onOpenChange, onSelect, onEmptyEnter, }) {
    const inputRef = useRef(null);
    const [query, setQuery] = useState("");
    const [selectedIndex, setSelectedIndex] = useState(0);
    const filteredItems = useMemo(() => filterItems(items, query), [filterItems, items, query]);
    const activeItem = filteredItems[selectedIndex];
    useEffect(() => {
        if (!open)
            return;
        setQuery("");
        setSelectedIndex(0);
        const timer = window.setTimeout(() => inputRef.current?.focus(), 0);
        return () => window.clearTimeout(timer);
    }, [open]);
    useEffect(() => {
        setSelectedIndex(0);
    }, [query]);
    useEffect(() => {
        if (filteredItems.length === 0) {
            setSelectedIndex(0);
        }
        else if (selectedIndex >= filteredItems.length) {
            setSelectedIndex(filteredItems.length - 1);
        }
    }, [filteredItems.length, selectedIndex]);
    const onInputKeyDown = (event) => {
        if (event.key === "Escape") {
            onOpenChange(false);
        }
        else if (event.key === "ArrowDown") {
            event.preventDefault();
            setSelectedIndex((index) => Math.min(index + 1, Math.max(0, filteredItems.length - 1)));
        }
        else if (event.key === "ArrowUp") {
            event.preventDefault();
            setSelectedIndex((index) => Math.max(index - 1, 0));
        }
        else if (event.key === "Enter") {
            event.preventDefault();
            if (activeItem !== undefined) {
                void onSelect(activeItem);
            }
            else if (onEmptyEnter) {
                void onEmptyEnter(query);
            }
        }
    };
    return {
        inputRef,
        query,
        setQuery,
        filteredItems,
        selectedIndex,
        setSelectedIndex,
        activeItem,
        activate: (item) => void onSelect(item),
        onInputKeyDown,
    };
}
