;; The cutout's one fast step, for browsers that run WebAssembly with SIMD (cutout.js falls back to plain JavaScript
;; where they do not, or where the page's policy forbids it). Built into cutout.js by scripts/cutout_wasm.js.
;;
;; conv1(x, y, n, cin, cout, w, b): a 1x1 convolution over planes (all of channel 0, then channel 1, ...).
;;   y[o][p] = b[o] + sum over i of w[o * cin + i] * x[i][p], for p < n. Every argument but n, cin and cout is a
;;   byte offset into the memory; every value is a float32.
(module
  (import "m" "mem" (memory 1))
  (func (export "conv1") (param $x i32) (param $y i32) (param $n i32) (param $cin i32) (param $cout i32) (param $w i32) (param $b i32)
    (local $o i32) (local $i i32) (local $q i32) (local $yo i32) (local $wo i32) (local $nb i32) (local $n4b i32)
    (local $x0 i32) (local $x1 i32) (local $x2 i32) (local $x3 i32)
    (local $w0 v128) (local $w1 v128) (local $w2 v128) (local $w3 v128) (local $bv f32) (local $s f32)
    (local.set $nb (i32.shl (local.get $n) (i32.const 2)))
    (local.set $n4b (i32.shl (i32.and (local.get $n) (i32.const -4)) (i32.const 2)))
    (local.set $o (i32.const 0))
    (block $done_o (loop $each_o
      (br_if $done_o (i32.ge_u (local.get $o) (local.get $cout)))
      (local.set $yo (i32.add (local.get $y) (i32.mul (local.get $o) (local.get $nb))))
      (local.set $wo (i32.add (local.get $w) (i32.shl (i32.mul (local.get $o) (local.get $cin)) (i32.const 2))))
      ;; the bias
      (local.set $bv (f32.load (i32.add (local.get $b) (i32.shl (local.get $o) (i32.const 2)))))
      (local.set $w0 (f32x4.splat (local.get $bv)))
      (local.set $q (i32.const 0))
      (block $d1 (loop $l1
        (br_if $d1 (i32.ge_u (local.get $q) (local.get $n4b)))
        (v128.store (i32.add (local.get $yo) (local.get $q)) (local.get $w0))
        (local.set $q (i32.add (local.get $q) (i32.const 16)))
        (br $l1)))
      (block $d2 (loop $l2
        (br_if $d2 (i32.ge_u (local.get $q) (local.get $nb)))
        (f32.store (i32.add (local.get $yo) (local.get $q)) (local.get $bv))
        (local.set $q (i32.add (local.get $q) (i32.const 4)))
        (br $l2)))
      ;; four input channels at a time
      (local.set $i (i32.const 0))
      (block $done_i (loop $each_i
        (br_if $done_i (i32.gt_u (i32.add (local.get $i) (i32.const 4)) (local.get $cin)))
        (local.set $w0 (v128.load32_splat (i32.add (local.get $wo) (i32.shl (local.get $i) (i32.const 2)))))
        (local.set $w1 (v128.load32_splat offset=4 (i32.add (local.get $wo) (i32.shl (local.get $i) (i32.const 2)))))
        (local.set $w2 (v128.load32_splat offset=8 (i32.add (local.get $wo) (i32.shl (local.get $i) (i32.const 2)))))
        (local.set $w3 (v128.load32_splat offset=12 (i32.add (local.get $wo) (i32.shl (local.get $i) (i32.const 2)))))
        (local.set $x0 (i32.add (local.get $x) (i32.mul (local.get $i) (local.get $nb))))
        (local.set $x1 (i32.add (local.get $x0) (local.get $nb)))
        (local.set $x2 (i32.add (local.get $x1) (local.get $nb)))
        (local.set $x3 (i32.add (local.get $x2) (local.get $nb)))
        (local.set $q (i32.const 0))
        (block $d3 (loop $l3
          (br_if $d3 (i32.ge_u (local.get $q) (local.get $n4b)))
          (v128.store (i32.add (local.get $yo) (local.get $q))
            (f32x4.add
              (f32x4.add (v128.load (i32.add (local.get $yo) (local.get $q)))
                         (f32x4.mul (local.get $w0) (v128.load (i32.add (local.get $x0) (local.get $q)))))
              (f32x4.add
                (f32x4.add (f32x4.mul (local.get $w1) (v128.load (i32.add (local.get $x1) (local.get $q))))
                           (f32x4.mul (local.get $w2) (v128.load (i32.add (local.get $x2) (local.get $q)))))
                (f32x4.mul (local.get $w3) (v128.load (i32.add (local.get $x3) (local.get $q)))))))
          (local.set $q (i32.add (local.get $q) (i32.const 16)))
          (br $l3)))
        (block $d4 (loop $l4
          (br_if $d4 (i32.ge_u (local.get $q) (local.get $nb)))
          (f32.store (i32.add (local.get $yo) (local.get $q))
            (f32.add (f32.load (i32.add (local.get $yo) (local.get $q)))
              (f32.add
                (f32.add (f32.mul (f32x4.extract_lane 0 (local.get $w0)) (f32.load (i32.add (local.get $x0) (local.get $q))))
                         (f32.mul (f32x4.extract_lane 0 (local.get $w1)) (f32.load (i32.add (local.get $x1) (local.get $q)))))
                (f32.add (f32.mul (f32x4.extract_lane 0 (local.get $w2)) (f32.load (i32.add (local.get $x2) (local.get $q))))
                         (f32.mul (f32x4.extract_lane 0 (local.get $w3)) (f32.load (i32.add (local.get $x3) (local.get $q))))))))
          (local.set $q (i32.add (local.get $q) (i32.const 4)))
          (br $l4)))
        (local.set $i (i32.add (local.get $i) (i32.const 4)))
        (br $each_i)))
      ;; the channels left over, one at a time
      (block $done_r (loop $each_r
        (br_if $done_r (i32.ge_u (local.get $i) (local.get $cin)))
        (local.set $s (f32.load (i32.add (local.get $wo) (i32.shl (local.get $i) (i32.const 2)))))
        (local.set $w0 (f32x4.splat (local.get $s)))
        (local.set $x0 (i32.add (local.get $x) (i32.mul (local.get $i) (local.get $nb))))
        (local.set $q (i32.const 0))
        (block $d5 (loop $l5
          (br_if $d5 (i32.ge_u (local.get $q) (local.get $n4b)))
          (v128.store (i32.add (local.get $yo) (local.get $q))
            (f32x4.add (v128.load (i32.add (local.get $yo) (local.get $q)))
                       (f32x4.mul (local.get $w0) (v128.load (i32.add (local.get $x0) (local.get $q))))))
          (local.set $q (i32.add (local.get $q) (i32.const 16)))
          (br $l5)))
        (block $d6 (loop $l6
          (br_if $d6 (i32.ge_u (local.get $q) (local.get $nb)))
          (f32.store (i32.add (local.get $yo) (local.get $q))
            (f32.add (f32.load (i32.add (local.get $yo) (local.get $q)))
                     (f32.mul (local.get $s) (f32.load (i32.add (local.get $x0) (local.get $q))))))
          (local.set $q (i32.add (local.get $q) (i32.const 4)))
          (br $l6)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $each_r)))
      (local.set $o (i32.add (local.get $o) (i32.const 1)))
      (br $each_o)))
  )
)
