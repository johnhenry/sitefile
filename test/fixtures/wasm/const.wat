;; Returns {"text":"hello from wasm"} for any request.
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "{\"text\":\"hello from wasm\"}")
  (func (export "sitefile_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "sitefile_api_version") (result i32) (i32.const 1))
  (func (export "sitefile_transform") (param i32 i32) (result i64)
    ;; (ptr 0 << 32) | len 26
    (i64.const 26)))
