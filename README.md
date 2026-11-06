# π in one minute

Computes as many digits of π as it can in 60 seconds (or any time limit you choose) and
shows the result live in a GPU-rendered Windows GUI (egui on wgpu → Vulkan / Direct3D 12).

On an i7-14700KF it reaches about **250–265 million digits** in one minute.

## Download

Get `pi-minute.exe` from the [latest release](https://github.com/torgeirf/pi-minute/releases/latest)
and run it. No installer or extra DLLs are needed. Requires 64-bit Windows 10/11 with
Direct3D 12 or Vulkan. The exe isn't code-signed, so SmartScreen may ask you to confirm
the first time ("More info" → "Run anyway").

## Choosing the time limit

The default is 60 seconds; anything from 1 to 3600 seconds works. Runs are planned to
finish just before the limit. Very short limits can run over by about a tenth of a second,
because the last step can't be split further.

**GUI:** set **Time limit** in the top-right corner and press **Run**. The field can be
edited once a run has finished. A run starts automatically when the app opens, using the
command-line value if one is given:

```
pi-minute.exe --seconds 30
pi-minute.exe 30              # same thing
```

**CLI** (`pi-cli.exe`, prints progress to the console):

```
pi-cli.exe --seconds 30                 # or: -s 30, --seconds=30, or just 30
pi-cli.exe -s 120 --out pi.txt          # also save the digits ("3.1415…")
pi-cli.exe --help
```

## Building

```
cargo run --release                          # GUI
cargo run --release -- --seconds 30          # GUI with a 30 s limit
cargo run --release --bin pi-cli -- -s 30    # CLI
```

`.cargo/config.toml` links the C runtime statically, so `target/release/pi-minute.exe`
is a self-contained portable exe.

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

`src/engine.rs` is the math; `src/main.rs` is the GUI; `src/bin/pi-cli.rs` is the headless
runner; `src/lib.rs` has the shared argument parsing.

## License

[MIT](LICENSE). Dependencies keep their own licenses; notably the malachite big-integer
library is LGPL-3.0, and the release executables link it statically. This repository is
the complete source needed to rebuild them.
