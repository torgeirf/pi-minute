//! Pi engine: Chudnovsky series evaluated with incremental, parallel binary splitting.
//!
//! Three things run at once:
//! * the **series thread** (main rayon pool, all cores but one) keeps extending the series
//!   state (P, Q, T) for terms [0, n) in growing blocks — no work is ever recomputed;
//! * the **preview thread** (small separate pool) repeatedly turns the newest series state
//!   into decimal digits so the GUI can show them while the series keeps growing;
//! * at the end, the **final pass** converts the largest state with every core, overlapping
//!   √10005 and the powers of ten with the last series block.
//!
//! Block sizes are chosen from measured timings so the final result lands right at the end
//! of the time budget.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use malachite_base::num::arithmetic::traits::{DivRem, FloorSqrt, Pow};
use malachite_base::num::logic::traits::SignificantBits;
use malachite_nz::integer::Integer;
use malachite_nz::natural::Natural;

const A: u64 = 13_591_409;
const B: u64 = 545_140_134;
/// 640320^3 / 24
const C3_OVER_24: u64 = 10_939_058_860_032_000;
/// Decimal digits gained per series term: log10(640320^3 / 1728).
pub const DIGITS_PER_TERM: f64 = 14.181_647_462_725_477;
const LOG2_10: f64 = 3.321_928_094_887_362;
/// Digits computed but not shown, to absorb truncation error in the last places.
const GUARD_DIGITS: u64 = 8;

/// Ranges at least this long are split across threads.
const PAR_SPLIT_TERMS: u64 = 512;
/// Ranges at least this long also run their 4 merge multiplications in parallel.
const PAR_MUL_TERMS: u64 = 16_384;
/// Safety margin applied to time predictions.
const SAFETY: f64 = 1.06;
/// Series size (~7M digits) at which the final-pass conversion speed is calibrated.
const CALIBRATE_TERMS: u64 = 512_000;
/// Size of the first series block (~28k digits).
const FIRST_BLOCK: u64 = 2_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Series { from: u64, to: u64 },
    Merge,
    Prepare,
    Sqrt,
    Divide,
    ToDecimal,
    Done,
}

impl Phase {
    pub fn label(&self) -> String {
        match self {
            Phase::Idle => "Idle".into(),
            Phase::Series { from, to } => format!("Binary splitting terms {} → {}", group(*from), group(*to)),
            Phase::Merge => "Merging block into series".into(),
            Phase::Prepare => "Scaling Q and T".into(),
            Phase::Sqrt => "√10005 and powers of ten".into(),
            Phase::Divide => "Long division".into(),
            Phase::ToDecimal => "Binary → decimal".into(),
            Phase::Done => "Done".into(),
        }
    }
}

/// Formats an integer with thin thousands separators.
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// One finished result that the GUI can display.
pub struct Snapshot {
    /// ASCII decimal digits after "3." (guard digits already removed).
    pub digits: Arc<Vec<u8>>,
    pub terms: u64,
    pub index: u32,
    /// Seconds since start when this result became available.
    pub at: f64,
    /// Seconds the binary → decimal pass took for this result.
    pub convert_secs: f64,
    /// How many leading digits agree with the previous result.
    pub verified: u64,
    pub histogram: [u64; 10],
    pub is_final: bool,
    pub timing: Timing,
}

/// Wall-clock seconds spent in each part of a conversion.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timing {
    pub prep: f64,
    pub divide: f64,
    pub decimal: f64,
}

pub struct Shared {
    pub start: Instant,
    pub budget: Duration,
    pub threads: usize,
    pub stop: AtomicBool,
    pub done: AtomicBool,
    /// Terms finished in the series block currently being computed.
    pub block_done: AtomicU64,
    pub block_total: AtomicU64,
    /// Terms contained in the merged series state.
    pub series_terms: AtomicU64,
    pub series_phase: Mutex<Phase>,
    pub convert_phase: Mutex<Phase>,
    pub final_pass: AtomicBool,
    pub latest: Mutex<Option<Arc<Snapshot>>>,
    /// (seconds, digits) for every result.
    pub history: Mutex<Vec<(f64, u64)>>,
    /// (seconds, digits) of the series state (what the digits will be once converted).
    pub series_history: Mutex<Vec<(f64, u64)>>,
    pub finished_at: Mutex<Option<f64>>,
    pub generation: AtomicU64,
    pending: Mutex<Option<Arc<Ratio>>>,
    pending_cv: Condvar,
    preview_stop: AtomicBool,
}

impl Shared {
    pub fn elapsed(&self) -> f64 {
        match *self.finished_at.lock().unwrap() {
            Some(t) => t,
            None => self.start.elapsed().as_secs_f64(),
        }
    }
    fn series(&self, p: Phase) {
        *self.series_phase.lock().unwrap() = p;
    }
    fn convert(&self, p: Phase, preview: bool) {
        // Once the final pass has started, a still-running preview must not overwrite its phase.
        if preview && self.final_pass.load(Ordering::Acquire) {
            return;
        }
        *self.convert_phase.lock().unwrap() = p;
    }
}

/// Asks Windows to run us at full speed: no EcoQoS/power throttling (which parks work on
/// efficiency cores on hybrid CPUs when the window isn't focused) and above-normal priority.
fn full_speed() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::*;
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
        SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }
}

/// Starts the computation in the background and returns the shared state.
pub fn start(budget: Duration) -> Arc<Shared> {
    full_speed();
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(1)
        .max(1);
    let shared = Arc::new(Shared {
        start: Instant::now(),
        budget,
        threads,
        stop: AtomicBool::new(false),
        done: AtomicBool::new(false),
        block_done: AtomicU64::new(0),
        block_total: AtomicU64::new(1),
        series_terms: AtomicU64::new(0),
        series_phase: Mutex::new(Phase::Idle),
        convert_phase: Mutex::new(Phase::Idle),
        final_pass: AtomicBool::new(false),
        latest: Mutex::new(None),
        history: Mutex::new(Vec::new()),
        series_history: Mutex::new(Vec::new()),
        finished_at: Mutex::new(None),
        generation: AtomicU64::new(0),
        pending: Mutex::new(None),
        pending_cv: Condvar::new(),
        preview_stop: AtomicBool::new(false),
    });

    let s = shared.clone();
    std::thread::Builder::new()
        .name("pi-series".into())
        .stack_size(64 << 20)
        .spawn(move || {
            let pool = pool(threads, "pi-worker");
            let preview_pool = pool_arc(threads.clamp(1, 4), "pi-preview");
            let ps = s.clone();
            std::thread::Builder::new()
                .name("pi-preview".into())
                .stack_size(64 << 20)
                .spawn(move || preview_loop(&ps, &preview_pool))
                .expect("spawn preview thread");
            pool.install(|| run(&s));
            *s.finished_at.lock().unwrap() = Some(s.start.elapsed().as_secs_f64());
            s.series(Phase::Done);
            s.convert(Phase::Done, false);
            s.done.store(true, Ordering::Release);
        })
        .expect("spawn series thread");
    shared
}

fn pool(threads: usize, name: &'static str) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(64 << 20)
        .thread_name(move |i| format!("{name}-{i}"))
        .build()
        .expect("thread pool")
}

fn pool_arc(threads: usize, name: &'static str) -> Arc<rayon::ThreadPool> {
    Arc::new(pool(threads, name))
}

// ---------------------------------------------------------------------------------------
// Binary splitting
// ---------------------------------------------------------------------------------------

struct Pqt {
    p: Integer,
    q: Integer,
    t: Integer,
}

fn leaf(a: u64) -> Pqt {
    if a == 0 {
        return Pqt { p: Integer::from(1u32), q: Integer::from(1u32), t: Integer::from(A) };
    }
    let ai = a as i128;
    let p = Integer::from(-(6 * ai - 5) * (2 * ai - 1) * (6 * ai - 1));
    let a3 = (a as u128).pow(3);
    let q = match a3.checked_mul(C3_OVER_24 as u128) {
        Some(v) => Integer::from(v),
        None => Integer::from(a3) * Integer::from(C3_OVER_24),
    };
    let t = &p * Integer::from(A as i128 + B as i128 * ai);
    Pqt { p, q, t }
}

/// Combines [a, m) with [m, b).
fn merge(l: Pqt, r: Pqt, parallel: bool) -> Pqt {
    if parallel {
        let ((p, q), (t1, t2)) = rayon::join(
            || rayon::join(|| &l.p * &r.p, || &l.q * &r.q),
            || rayon::join(|| &r.q * &l.t, || &l.p * &r.t),
        );
        Pqt { p, q, t: t1 + t2 }
    } else {
        let t = &r.q * &l.t + &l.p * &r.t;
        Pqt { p: l.p * r.p, q: l.q * r.q, t }
    }
}

fn split(a: u64, b: u64, s: &Shared) -> Pqt {
    let n = b - a;
    if n == 1 {
        s.block_done.fetch_add(1, Ordering::Relaxed);
        return leaf(a);
    }
    let m = a + n / 2;
    let (l, r) = if n >= PAR_SPLIT_TERMS {
        rayon::join(|| split(a, m, s), || split(m, b, s))
    } else {
        (split(a, m, s), split(m, b, s))
    };
    merge(l, r, n >= PAR_MUL_TERMS)
}

// ---------------------------------------------------------------------------------------
// Series state → decimal digits
// ---------------------------------------------------------------------------------------

/// Q/T truncated to the precision needed for `terms` terms.
struct Ratio {
    q: Natural,
    t: Natural,
    terms: u64,
}

fn digits_for(terms: u64) -> u64 {
    ((terms as f64) * DIGITS_PER_TERM) as u64
}

fn ratio(acc: &Pqt, terms: u64) -> Ratio {
    let keep_bits = (digits_for(terms) as f64 * LOG2_10) as u64 + 128;
    let q = acc.q.unsigned_abs_ref();
    let t = acc.t.unsigned_abs_ref();
    let shift = q.significant_bits().min(t.significant_bits()).saturating_sub(keep_bits);
    let (q, t) = rayon::join(|| q >> shift, || t >> shift);
    Ratio { q, t, terms }
}

/// Everything in the conversion that depends only on the digit count.
struct Prep {
    d: u64,
    /// floor(sqrt(10005) · 10^d)
    sqrt: Natural,
    /// Powers of ten for the parallel decimal conversion.
    radix: Radix,
}

fn prepare(d: u64) -> Prep {
    let (sqrt, radix) = rayon::join(
        || {
            let ten_d = Natural::from(10u32).pow(d);
            (Natural::from(10005u32) * (&ten_d * &ten_d)).floor_sqrt()
        },
        || Radix::new(d as usize + 1),
    );
    Prep { d, sqrt, radix }
}

/// Converts the ratio into digits after "3.". `prep` must have been made for this ratio.
fn convert(r: &Ratio, prep: Prep, s: &Shared, preview: bool, timing: &mut Timing) -> Vec<u8> {
    let t0 = Instant::now();
    s.convert(Phase::Divide, preview);
    let q = &r.q * Natural::from(426_880u32);
    // pi · 10^d = 426880 · sqrt(10005 · 10^2d) · Q / T
    let pi = (prep.sqrt * q) / &r.t;
    timing.divide = t0.elapsed().as_secs_f64();
    let t1 = Instant::now();
    s.convert(Phase::ToDecimal, preview);
    let mut bytes = prep.radix.to_decimal(&pi);
    timing.decimal = t1.elapsed().as_secs_f64();
    debug_assert_eq!(bytes[0], b'3');
    bytes.remove(0);
    bytes.truncate(prep.d.saturating_sub(GUARD_DIGITS) as usize);
    bytes
}

/// Parallel divide-and-conquer binary → decimal conversion of a number with `width` digits.
struct Radix {
    width: usize,
    leaf: usize,
    /// pows[i] = 10^(leaf · 2^i)
    pows: Vec<Natural>,
}

impl Radix {
    const MIN_LEAF: usize = 1 << 17;
    const MAX_LEVELS: usize = 8;

    fn new(width: usize) -> Radix {
        let mut levels = 0;
        while levels < Self::MAX_LEVELS && (Self::MIN_LEAF << (levels + 1)) <= width {
            levels += 1;
        }
        let leaf = width.div_ceil(1 << levels);
        let mut pows = Vec::with_capacity(levels);
        if levels > 0 {
            pows.push(Natural::from(10u32).pow(leaf as u64));
            for i in 1..levels {
                let p = &pows[i - 1] * &pows[i - 1];
                pows.push(p);
            }
        }
        Radix { width, leaf, pows }
    }

    fn to_decimal(&self, n: &Natural) -> Vec<u8> {
        let levels = self.pows.len();
        let total = self.leaf << levels;
        let mut out = vec![b'0'; total];
        self.rec(n, levels, &mut out);
        out.drain(..total - self.width);
        out
    }

    fn rec(&self, n: &Natural, level: usize, out: &mut [u8]) {
        if level == 0 {
            let s = n.to_string();
            let off = out.len() - s.len();
            out[off..].copy_from_slice(s.as_bytes());
            return;
        }
        let (hi, lo) = n.div_rem(&self.pows[level - 1]);
        let (a, b) = out.split_at_mut(out.len() / 2);
        rayon::join(|| self.rec(&hi, level - 1, a), || self.rec(&lo, level - 1, b));
    }
}

fn publish(s: &Shared, digits: Vec<u8>, terms: u64, convert_secs: f64, is_final: bool, timing: Timing) {
    let prev = s.latest.lock().unwrap().clone();
    if prev.as_ref().is_some_and(|p| p.terms >= terms) {
        return; // a preview finishing late must not replace a newer, larger result
    }
    let digits = Arc::new(digits);
    let verified = prev.as_ref().map_or(0, |p| common_prefix(&p.digits, &digits) as u64);
    let index = prev.as_ref().map_or(1, |p| p.index + 1);
    let histogram = histogram(&digits);
    let at = s.start.elapsed().as_secs_f64();
    let count = digits.len() as u64;
    *s.latest.lock().unwrap() = Some(Arc::new(Snapshot {
        digits,
        terms,
        index,
        at,
        convert_secs,
        verified,
        histogram,
        is_final,
        timing,
    }));
    s.history.lock().unwrap().push((at, count));
    s.generation.fetch_add(1, Ordering::Release);
}

/// Keeps converting the newest series state into digits for the display.
fn preview_loop(s: &Shared, pool: &rayon::ThreadPool) {
    loop {
        let job = {
            let mut g = s.pending.lock().unwrap();
            loop {
                if s.preview_stop.load(Ordering::Acquire) {
                    return;
                }
                if let Some(j) = g.take() {
                    break j;
                }
                g = s.pending_cv.wait(g).unwrap();
            }
        };
        let t0 = Instant::now();
        let stopped = pool.install(|| {
            let mut timing = Timing::default();
            s.convert(Phase::Sqrt, true);
            let prep = prepare(digits_for(job.terms));
            timing.prep = t0.elapsed().as_secs_f64();
            let digits = convert(&job, prep, s, true, &mut timing);
            if s.preview_stop.load(Ordering::Acquire) {
                return true; // the final result is on its way; don't publish a stale one after it
            }
            publish(s, digits, job.terms, t0.elapsed().as_secs_f64(), false, timing);
            s.convert(Phase::Idle, true);
            false
        });
        if stopped {
            return;
        }
    }
}

/// Rough cost model: work grows like n·log²n for FFT-based arithmetic.
fn work(terms: u64) -> f64 {
    let n = terms.max(2) as f64;
    let l = (n * DIGITS_PER_TERM).ln();
    n * l * l
}

fn run(s: &Shared) {
    let budget = s.budget.as_secs_f64();
    let mut acc: Option<Pqt> = None;
    let mut n: u64 = 0;
    let mut next: u64 = FIRST_BLOCK;
    // Seconds per unit of `work` for extending the series / for the final conversion.
    let mut c_series = 0.0f64;
    let mut c_prep = 0.0f64;
    let mut c_conv = 0.0f64;
    // √10005 + powers of ten on the full pool with nothing else running (from calibration).
    let mut c_prep_full = 0.0f64;
    let mut last = false;
    // Whether the last block computes √10005 etc. alongside the series (else in the final pass).
    let mut overlap = true;
    let mut prep: Option<Prep> = None;

    while !s.stop.load(Ordering::Relaxed) {
        // --- extend the series by [n, next) ---
        let t0 = Instant::now();
        s.block_done.store(0, Ordering::Relaxed);
        s.block_total.store(next - n, Ordering::Relaxed);
        s.series(Phase::Series { from: n, to: next });
        let block = if last && overlap {
            // Overlap the final conversion's setup with the last block.
            let d = digits_for(next);
            let (block, p) = rayon::join(|| split(n, next, s), || prepare(d));
            prep = Some(p);
            block
        } else {
            split(n, next, s)
        };
        s.series(Phase::Merge);
        let merged = match acc.take() {
            None => block,
            Some(prev) => merge(prev, block, true),
        };
        let series_secs = t0.elapsed().as_secs_f64();
        let w = (work(next) - work(n)).max(1.0);
        if series_secs > 0.02 || c_series == 0.0 {
            c_series = if c_series == 0.0 { series_secs / w } else { 0.3 * c_series + 0.7 * series_secs / w };
        }
        n = next;
        s.series_terms.store(n, Ordering::Relaxed);
        s.series_history
            .lock()
            .unwrap()
            .push((s.start.elapsed().as_secs_f64(), digits_for(n).saturating_sub(GUARD_DIGITS)));

        if last {
            acc = Some(merged);
            break;
        }

        // Hand the new state to the preview thread (replacing any state it hasn't started).
        let r = Arc::new(ratio(&merged, n));
        acc = Some(merged);
        *s.pending.lock().unwrap() = Some(r);
        s.pending_cv.notify_one();

        if c_conv == 0.0 && n >= CALIBRATE_TERMS {
            // One conversion on the full pool with nothing competing, exactly like the final
            // pass, to learn how fast that will be. The result is shown like any other.
            let t0 = Instant::now();
            s.convert(Phase::Sqrt, false);
            let prep = prepare(digits_for(n));
            let mut timing = Timing { prep: t0.elapsed().as_secs_f64(), ..Timing::default() };
            let r = ratio(acc.as_ref().unwrap(), n);
            let digits = convert(&r, prep, s, false, &mut timing);
            c_conv = (timing.divide + timing.decimal) / work(n);
            c_prep_full = timing.prep / work(n);
            if c_prep == 0.0 {
                c_prep = timing.prep / work(n);
            }
            publish(s, digits, n, t0.elapsed().as_secs_f64(), false, timing);
            s.convert(Phase::Idle, false);
        }
        // √10005 and the powers of ten get computed alongside the last block, sharing the CPU
        // with it, which is what the previews experience too: calibrate from them.
        if let Some(snap) = s.latest.lock().unwrap().as_ref() {
            if !snap.is_final && snap.timing.prep > 0.05 {
                c_prep = snap.timing.prep / work(snap.terms);
            }
        }
        let (cp, cpf, cc) = if c_conv > 0.0 {
            (c_prep, c_prep_full, c_conv)
        } else {
            (c_series, c_series, 2.0 * c_series)
        };

        // --- plan the next block ---
        let remaining = budget - s.start.elapsed().as_secs_f64();
        let series_t = |a: u64, b: u64| c_series * (work(b) - work(a));
        // Time to run a last block a→m and then convert, either with √10005 etc. computed
        // alongside the block, or afterwards on all cores. Alongside, both share the same
        // cores, so they only partly overlap: measured ≈ the longer plus half the shorter.
        let finish_overlap = |a: u64, m: u64| {
            let (s, p) = (series_t(a, m), cp * work(m));
            SAFETY * (s.max(p) + 0.5 * s.min(p) + cc * work(m))
        };
        let finish_seq = |a: u64, m: u64| SAFETY * (series_t(a, m) + (cpf + cc) * work(m));
        let finish = |a: u64, m: u64| finish_overlap(a, m).min(finish_seq(a, m));
        let step = |m: u64| m + m / 50 + 1;
        let full = n * 2;
        if SAFETY * series_t(n, full) + finish(full, step(full)) <= remaining {
            // A full doubling still leaves room for a final block afterwards.
            next = full;
        } else if finish(n, step(n)) > remaining {
            break; // out of time: convert what we have
        } else {
            // This is the last block: make it as large as the remaining time allows.
            let (mut lo, mut hi) = (step(n), full);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if finish(n, mid) <= remaining {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            next = lo;
            last = true;
            overlap = finish_overlap(n, next) <= finish_seq(n, next);
        }
    }

    // --- final pass with every core ---
    s.preview_stop.store(true, Ordering::Release);
    s.pending_cv.notify_all();
    s.final_pass.store(true, Ordering::Release);
    s.series(Phase::Idle);
    let Some(acc) = acc else { return };
    let t0 = Instant::now();
    s.convert(Phase::Prepare, false);
    let r = ratio(&acc, n);
    drop(acc);
    let mut timing = Timing::default();
    let prep = match prep {
        Some(p) if p.d == digits_for(n) => p,
        _ => {
            s.convert(Phase::Sqrt, false);
            prepare(digits_for(n))
        }
    };
    timing.prep = t0.elapsed().as_secs_f64();
    let digits = convert(&r, prep, s, false, &mut timing);
    publish(s, digits, n, t0.elapsed().as_secs_f64(), true, timing);
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let chunk = 1 << 16;
    let mut i = 0;
    while i < n {
        let e = (i + chunk).min(n);
        if a[i..e] != b[i..e] {
            return i + a[i..e].iter().zip(&b[i..e]).take_while(|(x, y)| x == y).count();
        }
        i = e;
    }
    n
}

fn histogram(digits: &[u8]) -> [u64; 10] {
    use rayon::prelude::*;
    digits
        .par_chunks(1 << 20)
        .map(|c| {
            let mut h = [0u64; 10];
            for &d in c {
                h[(d - b'0') as usize] += 1;
            }
            h
        })
        .reduce(
            || [0u64; 10],
            |mut a, b| {
                for i in 0..10 {
                    a[i] += b[i];
                }
                a
            },
        )
}
