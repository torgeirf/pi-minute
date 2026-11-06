# π in one minute

Computes as many digits of π as it can in 60 seconds and shows the result live in a
GPU-rendered Windows GUI (egui on wgpu → Vulkan / Direct3D 12).

```
cargo run --release            # 60 s budget
cargo run --release -- 30      # custom budget in seconds
cargo run --release --bin pi-cli -- 60 --out pi.txt   # headless, prints a log, saves digits
```

On an i7-14700KF this reaches about **250–265 million digits** in the minute.

## How it works

* **Chudnovsky series**, about 14.18 digits per term, evaluated with **binary splitting**
  over [malachite](https://crates.io/crates/malachite) big integers (FFT multiplication).
* **Incremental:** the series state (P, Q, T) for terms [0, n) is kept and extended
  block by block, so no work is redone. Big subtrees and the 4 merge multiplications run
  in parallel on a rayon pool.
* **Pipelined display:** a separate preview thread keeps converting the newest series
  state to decimal for the screen while the series keeps growing.
* **Time planning:** block sizes come from measured timings (one calibration conversion
  at about 7M digits, plus the previews). The last block is sized so the final
  conversion ends just before the deadline. √10005 and the powers of ten for the final
  pass are computed alongside that last block.
* **Final pass:** sqrt, then a long division, then a parallel divide-and-conquer
  binary→decimal conversion.
* **Self-check:** each result is compared with the previous one, computed from a
  different number of terms, and the shared prefix is shown as "Matches previous".
* The process opts out of Windows power throttling (EcoQoS), so hybrid CPUs don't park
  the work on efficiency cores.

`src/engine.rs` is the math; `src/main.rs` is the GUI; `src/bin/pi-cli.rs` is the headless runner.
