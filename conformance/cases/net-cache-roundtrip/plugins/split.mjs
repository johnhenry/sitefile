export function transform(r) { return { files: { "one.txt": r.input.text + "1", "sub/two.txt": r.input.text + "2" } }; }
