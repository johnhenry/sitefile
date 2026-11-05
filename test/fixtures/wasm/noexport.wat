;; Missing sitefile_transform.
(module
  (memory (export "memory") 1)
  (func (export "sitefile_alloc") (param i32) (result i32) (i32.const 0)))
