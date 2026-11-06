pub mod engine;

/// Default time budget in seconds.
pub const DEFAULT_SECONDS: f64 = 60.0;
/// Allowed time budget range in seconds.
pub const MIN_SECONDS: f64 = 1.0;
pub const MAX_SECONDS: f64 = 3600.0;

/// Parses the time budget from the command line: `--seconds N`, `-s N`, `--seconds=N`
/// or a bare number. Returns the remaining arguments untouched.
pub fn parse_seconds(args: &[String]) -> Result<(f64, Vec<String>), String> {
    let mut secs = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let value = if a == "--seconds" || a == "-s" {
            Some(it.next().ok_or_else(|| format!("{a} needs a number of seconds"))?.as_str())
        } else if let Some(v) = a.strip_prefix("--seconds=") {
            Some(v)
        } else if a.parse::<f64>().is_ok() {
            Some(a.as_str())
        } else {
            rest.push(a.clone());
            None
        };
        if let Some(v) = value {
            let s: f64 = v.parse().map_err(|_| format!("'{v}' is not a number of seconds"))?;
            if !(MIN_SECONDS..=MAX_SECONDS).contains(&s) {
                return Err(format!("seconds must be between {MIN_SECONDS} and {MAX_SECONDS}, got {s}"));
            }
            secs = Some(s);
        }
    }
    Ok((secs.unwrap_or(DEFAULT_SECONDS), rest))
}
