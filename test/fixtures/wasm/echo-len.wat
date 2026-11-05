;; Proves the request arrives: returns {"text":"<n>"} where n is a digit derived
;; from the request length mod 10, written at offset 0.
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "{\"text\":\"0\"}")
  (func (export "sitefile_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "sitefile_transform") (param $p i32) (param $n i32) (result i64)
    (i32.store8 (i32.const 9) (i32.add (i32.const 48) (i32.rem_u (local.get $n) (i32.const 10))))
    (i64.const 12)))
