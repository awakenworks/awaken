import { useCallback, useEffect, useState } from "react";
function readDraft(key) {
    try {
        return globalThis.localStorage?.getItem(key) ?? "";
    }
    catch {
        return "";
    }
}
function writeDraft(key, value) {
    try {
        if (value)
            globalThis.localStorage?.setItem(key, value);
        else
            globalThis.localStorage?.removeItem(key);
    }
    catch {
        // Draft persistence is best-effort when storage is unavailable.
    }
}
/** The product supplies a fully namespaced key, preventing cross-product collisions. */
export function useChatDraft(key) {
    const [value, setState] = useState(() => readDraft(key));
    useEffect(() => setState(readDraft(key)), [key]);
    const setValue = useCallback((next) => {
        setState(next);
        writeDraft(key, next);
    }, [key]);
    const clear = useCallback(() => {
        setState("");
        writeDraft(key, "");
    }, [key]);
    return { value, setValue, clear };
}
