;; Traps.
(module
  (memory (export "memory") 1)
  (func (export "sitefile_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "sitefile_transform") (param i32 i32) (result i64) (unreachable)))
