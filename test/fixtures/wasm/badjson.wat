;; Returns text that isn't JSON.
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "not json")
  (func (export "sitefile_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "sitefile_transform") (param i32 i32) (result i64) (i64.const 8)))
