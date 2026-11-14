import { useEffect, useEffectEvent, useState } from "react";

/** Returns `value` after it has stopped changing for `ms`. */
export function useDebouncedValue<T>(value: T, ms = 400): T {
  const [debounced, setDebounced] = useState(value);
  // External system: a browser timer that delays the update; cleared when value changes or on unmount.
  useEffect(() => {
    const id = setTimeout(() => setDebounced(value), ms);
    return () => clearTimeout(id);
  }, [value, ms]);
  return debounced;
}

/** Listen to window keydown; the handler always sees the latest props/state. */
export function useWindowKeydown(handler: (e: KeyboardEvent) => void) {
  const onKey = useEffectEvent(handler);
  // External system: window keydown events; listener removed on cleanup.
  useEffect(() => {
    const listener = (e: KeyboardEvent) => onKey(e);
    window.addEventListener("keydown", listener);
    return () => window.removeEventListener("keydown", listener);
  }, []);
}

export function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName);
}
