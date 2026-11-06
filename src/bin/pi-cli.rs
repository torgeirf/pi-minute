//! Headless runner: `pi-cli [--seconds N] [--out pi.txt]`
use std::sync::atomic::Ordering;
use std::time::Duration;

use pi_minute::engine;

const HELP: &str = "\
pi-cli: compute as many digits of pi as possible within a time limit

USAGE:
    pi-cli [OPTIONS] [SECONDS]

OPTIONS:
    -s, --seconds <N>   Time limit in seconds, 1-3600 (default 60). A bare number works too.
    -o, --out <FILE>    Write the final digits (\"3.1415...\") to FILE
    -h, --help          Show this help";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return;
    }
    let (secs, rest) = pi_minute::parse_seconds(&args).unwrap_or_else(|e| fail(&e));
    let mut out = None;
    let mut it = rest.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "--out" => out = Some(it.next().unwrap_or_else(|| fail(&format!("{a} needs a file name")))),
            _ => fail(&format!("unknown argument '{a}'")),
        }
    }

    println!("computing pi for {secs} s on {} threads...", std::thread::available_parallelism().map_or(1, |n| n.get()));
    let s = engine::start(Duration::from_secs_f64(secs));
    let (mut seen, mut series_seen) = (0, 0);
    loop {
        std::thread::sleep(Duration::from_millis(20));
        {
            let h = s.series_history.lock().unwrap();
            for &(t, d) in &h[series_seen..] {
                println!("{t:6.2}s   series state now worth {:>13} digits", engine::group(d));
            }
            series_seen = h.len();
        }
        let g = s.generation.load(Ordering::Acquire);
        if g != seen {
            seen = g;
            if let Some(snap) = s.latest.lock().unwrap().clone() {
                let d = &snap.digits;
                let tail = String::from_utf8_lossy(&d[d.len().saturating_sub(16)..]);
                println!(
                    "{:6.2}s {} #{:<2} {:>13} digits (convert {:6.2}s = prep {:5.2} div {:5.2} dec {:5.2})  verified {:>13}  ...{}",
                    snap.at,
                    if snap.is_final { "FINAL " } else { "result" },
                    snap.index,
                    engine::group(d.len() as u64),
                    snap.convert_secs,
                    snap.timing.prep,
                    snap.timing.divide,
                    snap.timing.decimal,
                    engine::group(snap.verified),
                    tail
                );
            }
        }
        if s.done.load(Ordering::Acquire) && s.generation.load(Ordering::Acquire) == seen {
            break;
        }
    }
    let snap = s.latest.lock().unwrap().clone().unwrap();
    println!("finished at {:.2}s with {} digits", s.elapsed(), engine::group(snap.digits.len() as u64));
    if let Some(path) = out {
        let mut text = b"3.".to_vec();
        text.extend_from_slice(&snap.digits);
        std::fs::write(&path, text).unwrap_or_else(|e| fail(&format!("could not write {path}: {e}")));
        println!("wrote {path}");
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}\n\n{HELP}");
    std::process::exit(2);
}
