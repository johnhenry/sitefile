// Keeps every object, so the heap only grows.
export function transform() {
  const keep = [];
  for (;;) keep.push({ n: keep.length, s: String(keep.length) });
}
