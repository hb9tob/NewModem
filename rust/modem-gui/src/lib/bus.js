// Shared layer — a tiny synchronous event bus that breaks tabs/x -> tabs/y
// direct imports, keeping the module graph acyclic. A tab `emit`s an intent
// (e.g. "tx:recompress", "settings:loaded", "sdr:refresh") and any tab that
// owns the reaction subscribes with `on` in its setup(). Imports nothing.

const handlers = new Map();

export function on(event, fn) {
  let set = handlers.get(event);
  if (!set) {
    set = new Set();
    handlers.set(event, set);
  }
  set.add(fn);
  return () => set.delete(fn);
}

export function emit(event, ...args) {
  const set = handlers.get(event);
  if (!set) return;
  for (const fn of [...set]) {
    try {
      fn(...args);
    } catch (err) {
      console.error(`bus handler for "${event}" threw`, err);
    }
  }
}
