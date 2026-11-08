;; Grows its memory by 32 pages (2 MiB) and returns {"text":"grew"}; traps when the grow is refused.
;; With a 1 MiB --plugin-memory, wasmtime refuses the grow (unreachable) and V8 fails the call after it.
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "{\"text\":\"grew\"}")
  (func (export "sitefile_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "sitefile_transform") (param i32 i32) (result i64)
    (if (i32.eq (memory.grow (i32.const 32)) (i32.const -1)) (then unreachable))
    ;; (ptr 0 << 32) | len 15
    (i64.const 15)))
