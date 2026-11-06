//! Headless runner: `pi-cli [seconds] [--out pi.txt]`
use std::sync::atomic::Ordering;
use std::time::Duration;

use pi_minute::engine;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: f64 = args.iter().find_map(|a| a.parse().ok()).unwrap_or(60.0);
    let out = args.iter().position(|a| a == "--out").and_then(|i| args.get(i + 1)).cloned();

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
        std::fs::write(&path, text).expect("write output");
        println!("wrote {path}");
    }
}
