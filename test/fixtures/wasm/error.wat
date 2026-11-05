;; Reports an error.
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "{\"error\":\"boom: bad input\"}")
  (func (export "sitefile_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "sitefile_transform") (param i32 i32) (result i64) (i64.const 27)))
