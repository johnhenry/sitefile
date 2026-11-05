;; Not allowed: imports a host function.
(module
  (import "env" "now" (func $now (result i64)))
  (memory (export "memory") 1)
  (func (export "sitefile_alloc") (param i32) (result i32) (i32.const 0))
  (func (export "sitefile_transform") (param i32 i32) (result i64) (call $now)))
