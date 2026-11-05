;; Returns a result that points outside memory.
(module
  (memory (export "memory") 1)
  (func (export "sitefile_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "sitefile_transform") (param i32 i32) (result i64)
    ;; ptr 65000, len 10000 (> 64 KiB page)
    (i64.or (i64.shl (i64.const 65000) (i64.const 32)) (i64.const 10000))))
