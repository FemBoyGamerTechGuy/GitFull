//! Live clone progress UI.
//!
//! gitfull spawns `git clone --progress` and parses the progress lines git
//! emits on stderr (`Receiving objects: 45% (123/2743), 1.20 MiB | 3.4 MiB/s`),
//! then renders its **own** single-line, live-updating display:
//!
//! ```text
//! Receiving  [███████████░░░░░░░░░░░░░░░]  45%   1.2/2.7 MiB   3.4 MiB/s  ETA 00:12
//! ```
//!
//! * visually filling progress bar
//! * estimated time remaining (smoothed transfer rate + estimated total)
//! * data transferred so far / total-size estimate (git reports object
//!   counts and bytes-so-far; total bytes is estimated as bytes/pct)
//!
//! On a non-TTY stderr the bar degrades to milestone lines every 10%.

use std::io::Write;
use std::time::Instant;

use crate::util::{format_bytes, format_duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Enumerating,
    Counting,
    Compressing,
    Receiving,
    Resolving,
    Updating,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Enumerating => "Enumerating",
            Phase::Counting => "Counting",
            Phase::Compressing => "Compressing",
            Phase::Receiving => "Receiving",
            Phase::Resolving => "Resolving",
            Phase::Updating => "Updating",
        }
    }
}

/// One parsed git progress line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub phase: Option<Phase>,
    pub pct: Option<f64>,
    pub done: Option<u64>,
    pub total: Option<u64>,
    /// Bytes transferred so far.
    pub bytes: Option<f64>,
    /// Instantaneous transfer rate (bytes/sec).
    pub rate: Option<f64>,
}

/// Parse one raw git progress line (`remote: ` prefix already handled).
/// Returns `None` for lines that carry no progress payload.
pub fn parse_git_progress(raw: &str) -> Option<Sample> {
    let mut s = raw.trim_end_matches(['\r', '\n']);
    if let Some(r) = s.strip_prefix("remote: ") {
        s = r.trim_end_matches(['\r', '\n']);
    }
    let (phase, rest) = if let Some(r) = s.strip_prefix("Enumerating objects:") {
        (Phase::Enumerating, r)
    } else if let Some(r) = s.strip_prefix("Counting objects:") {
        (Phase::Counting, r)
    } else if let Some(r) = s.strip_prefix("Compressing objects:") {
        (Phase::Compressing, r)
    } else if let Some(r) = s.strip_prefix("Receiving objects:") {
        (Phase::Receiving, r)
    } else if let Some(r) = s.strip_prefix("Resolving deltas:") {
        (Phase::Resolving, r)
    } else if let Some(r) = s.strip_prefix("Updating files:") {
        (Phase::Updating, r)
    } else {
        return None;
    };

    let mut sample = Sample {
        phase: Some(phase),
        ..Default::default()
    };
    let mut rest = rest.trim();
    let done_marker = rest.ends_with(", done.");
    if done_marker {
        rest = rest.strip_suffix(", done.").unwrap_or(rest);
    }

    if let Some(pct_end) = rest.find('%') {
        if let Ok(p) = rest[..pct_end].trim().parse::<f64>() {
            sample.pct = Some(p);
        }
        rest = rest[pct_end + 1..].trim_start();
    }

    if rest.starts_with('(') {
        if let Some(close) = rest.find(')') {
            if let Some((a, b)) = rest[1..close].split_once('/') {
                if let (Ok(x), Ok(y)) = (a.trim().parse::<u64>(), b.trim().parse::<u64>()) {
                    sample.done = Some(x);
                    sample.total = Some(y);
                }
            }
            rest = rest[close + 1..].trim_start();
        }
    } else if sample.done.is_none() {
        // bare-count form: "Enumerating objects: 1234, done."
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let after = &rest[digits.len()..];
            if after.is_empty() || after.starts_with(',') {
                sample.done = Some(digits.parse().unwrap_or(0));
            }
        }
    }

    // optional size / rate payload: ", 1.20 MiB | 3.40 MiB/s" or "823 KiB"
    let payload = rest.trim().trim_start_matches(',').trim();
    if !payload.is_empty() && payload.contains(' ') {
        if let Some(bar) = payload.find('|') {
            sample.bytes = parse_size(payload[..bar].trim());
            let rate = payload[bar + 1..].trim();
            sample.rate = parse_size(rate.strip_suffix("/s").unwrap_or(rate));
        } else {
            sample.bytes = parse_size(payload);
        }
    }

    if done_marker {
        sample.pct = sample.pct.map(|p| p.max(100.0)).or(Some(100.0));
    }
    Some(sample)
}

/// Parse `"1.20 MiB"` / `"823 KiB"` / `"512"` into bytes.
pub fn parse_size(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = match s.split_once(' ') {
        Some((n, u)) => (n, u.trim()),
        None => (s, "B"),
    };
    let n: f64 = num.parse().ok()?;
    const K: f64 = 1024.0;
    let mult = match unit {
        "B" | "" => 1.0,
        "KiB" => K,
        "MiB" => K * K,
        "GiB" => K * K * K,
        "TiB" => K * K * K * K,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        _ => return None,
    };
    Some(n * mult)
}

/// Live progress renderer for one clone operation.
pub struct ProgressUi {
    tty: bool,
    width: usize,
    color: bool,
    start: Instant,
    last_draw: Instant,
    /// Smoothed transfer rate (EMA), bytes/sec.
    ema_rate: Option<f64>,
    /// Smoothed estimated total bytes.
    est_total: Option<f64>,
    last_bytes: Option<f64>,
    last_milestone: u8,
    drawn: bool,
}

impl ProgressUi {
    pub fn new(tty: bool, width: usize, color: bool) -> ProgressUi {
        ProgressUi {
            tty,
            width,
            color,
            start: Instant::now(),
            last_draw: Instant::now(),
            ema_rate: None,
            est_total: None,
            last_bytes: None,
            last_milestone: 0,
            drawn: false,
        }
    }

    /// Feed one parsed sample; redraws at most every 100ms on a TTY, or
    /// prints a milestone line every 10% when non-interactive.
    pub fn update(&mut self, sample: &Sample) {
        let now = Instant::now();

        if let (Some(bytes), Some(rate)) = (sample.bytes, sample.rate) {
            if rate > 0.0 {
                self.ema_rate = match self.ema_rate {
                    None => Some(rate),
                    Some(e) => Some(e + 0.25 * (rate - e)),
                };
            }
            self.last_bytes = Some(bytes);
        }
        if let (Some(bytes), Some(pct)) = (sample.bytes, sample.pct) {
            if pct > 1.0 {
                let est = bytes * 100.0 / pct;
                self.est_total = match self.est_total {
                    None => Some(est),
                    Some(e) => Some(e + 0.2 * (est - e)),
                };
            }
        }

        if self.tty {
            if now.duration_since(self.last_draw).as_millis() >= 100 {
                let line = self.render(sample);
                let mut err = std::io::stderr();
                let _ = err.write_all(b"\r\x1b[2K");
                let _ = err.write_all(line.as_bytes());
                let _ = err.flush();
                self.last_draw = now;
                self.drawn = true;
            }
        } else if let Some(pct) = sample.pct {
            let milestone = (pct / 10.0).floor() as u8;
            if milestone > self.last_milestone {
                self.last_milestone = milestone;
                let mut err = std::io::stderr();
                let extra = match (sample.bytes, self.ema_rate) {
                    (Some(b), Some(r)) => format!(", {}/{}/s", format_bytes(b), format_bytes(r)),
                    (Some(b), None) => format!(", {}", format_bytes(b)),
                    _ => String::new(),
                };
                let _ = writeln!(
                    err,
                    "gitfull: {} {pct:.0}%{extra}",
                    sample.phase.map(|p| p.label()).unwrap_or("clone")
                );
            }
        }
    }

    /// Estimated time remaining as a `mm:ss` string, or `--:--`.
    fn eta_text(&self) -> String {
        match (self.est_total, self.last_bytes, self.ema_rate) {
            (Some(total), Some(bytes), Some(rate)) if rate > 0.0 => {
                let remain = (total - bytes).max(0.0);
                let secs = (remain / rate).round() as u64;
                if secs > 60 * 60 * 24 {
                    "--:--".to_string()
                } else {
                    format_duration(secs)
                }
            }
            _ => "--:--".to_string(),
        }
    }

    /// Render the one-line display (public so tests can snapshot it).
    pub fn render(&self, sample: &Sample) -> String {
        let label = sample.phase.map(|p| p.label()).unwrap_or("clone");
        let elapsed = self.start.elapsed().as_secs_f64();

        let pct_txt = sample
            .pct
            .map(|p| format!("{p:>3.0}%"))
            .unwrap_or_else(|| "    ".into());
        let bytes_txt = match (sample.bytes, sample.pct) {
            (Some(b), Some(p)) if p > 0.5 => {
                let est = b * 100.0 / p;
                format!("{}/{}", format_bytes(b), format_bytes(est))
            }
            (Some(b), _) => format_bytes(b),
            _ => String::new(),
        };
        let counts_txt = if bytes_txt.is_empty() {
            match (sample.done, sample.total) {
                (Some(d), Some(t)) => format!("{d}/{t}"),
                (Some(d), None) => format!("{d}"),
                _ => String::new(),
            }
        } else {
            String::new()
        };
        let rate_txt = self
            .ema_rate
            .map(|r| format!("{}/s", format_bytes(r)))
            .unwrap_or_default();
        let eta_txt = format!("ETA {}", self.eta_text());

        // Budget: label(10) pct(4) bytes(13) rate(10) eta(12) + 6 spaces.
        let text_len = 10
            + 1
            + 4
            + 1
            + bytes_txt.chars().count().max(counts_txt.chars().count())
            + 1
            + rate_txt.chars().count()
            + 1
            + eta_txt.chars().count()
            + 4;
        let bar_w = self.width.saturating_sub(text_len).min(32);

        if bar_w < 8 {
            // Very narrow terminal: text-only.
            let parts: Vec<String> = [
                format!("{label:<10}"),
                pct_txt.trim().to_string(),
                bytes_txt,
                counts_txt,
                rate_txt,
                eta_txt,
            ]
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
            return parts.join(" ");
        }

        let filled = sample
            .pct
            .map(|p| ((bar_w as f64) * (p / 100.0)).round() as usize)
            .unwrap_or(0)
            .min(bar_w);
        let bar: String = "█".repeat(filled) + &"░".repeat(bar_w - filled);

        let line = format!(
            "{label:<10} {bar} {pct_txt} {bytes_txt}{counts_txt} {rate_txt} {eta_txt} {elapsed:>4.0}s"
        );
        if self.color {
            if let Some((s, e)) = bar_span(&line) {
                format!("{}\x1b[36m{}\x1b[0m{}", &line[..s], &line[s..e], &line[e..])
            } else {
                line
            }
        } else {
            line
        }
    }

    /// End the line: erase the bar, print a final message.
    pub fn finish(&mut self, msg: &str) {
        let mut err = std::io::stderr();
        if self.tty && self.drawn {
            let _ = err.write_all(b"\r\x1b[2K");
        }
        let _ = writeln!(err, "{msg}");
        let _ = err.flush();
        self.drawn = false;
    }
}

/// Byte range of the bar glyph run (`█`/`░`) inside a rendered line.
fn bar_span(line: &str) -> Option<(usize, usize)> {
    let start = line.find('█').or_else(|| line.find('░'))?;
    let end = line[start..]
        .find(' ')
        .map(|e| start + e)
        .unwrap_or(line.len());
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_receiving() {
        let s = parse_git_progress("Receiving objects:  45% (123/2743), 1.20 MiB | 3.40 MiB/s")
            .unwrap();
        assert_eq!(s.phase, Some(Phase::Receiving));
        assert_eq!(s.pct, Some(45.0));
        assert_eq!(s.done, Some(123));
        assert_eq!(s.total, Some(2743));
        let mib = 1024.0 * 1024.0;
        assert!((s.bytes.unwrap() - 1.20 * mib).abs() < 0.01);
        assert!((s.rate.unwrap() - 3.40 * mib).abs() < 0.01);
    }

    #[test]
    fn parse_remote_prefix_and_done() {
        let s = parse_git_progress("remote: Enumerating objects: 1234, done.").unwrap();
        assert_eq!(s.phase, Some(Phase::Enumerating));
        assert_eq!(s.done, Some(1234));
        assert_eq!(s.pct, Some(100.0));

        let s = parse_git_progress("remote: Counting objects: 100% (2743/2743), done.").unwrap();
        assert_eq!(s.phase, Some(Phase::Counting));
        assert_eq!(s.pct, Some(100.0));
        assert_eq!(s.total, Some(2743));
    }

    #[test]
    fn parse_other_phases() {
        let s = parse_git_progress("Resolving deltas:  30% (456/1520)").unwrap();
        assert_eq!(s.phase, Some(Phase::Resolving));
        assert_eq!(s.pct, Some(30.0));
        let s = parse_git_progress("Updating files:  100% (500/500)").unwrap();
        assert_eq!(s.phase, Some(Phase::Updating));
        let s = parse_git_progress("Compressing objects:  55% (61/111)").unwrap();
        assert_eq!(s.phase, Some(Phase::Compressing));
        assert!(parse_git_progress("Cloning into 'x'...").is_none());
        assert!(parse_git_progress("warning: something").is_none());
    }

    #[test]
    fn parse_no_pct_with_size() {
        let s = parse_git_progress("Receiving objects: 823 KiB | 2.00 MiB/s").unwrap();
        assert_eq!(s.phase, Some(Phase::Receiving));
        assert!((s.bytes.unwrap() - 823.0 * 1024.0).abs() < 0.01);
    }

    #[test]
    fn render_contains_pieces() {
        let ui = ProgressUi::new(true, 100, false);
        let s = Sample {
            phase: Some(Phase::Receiving),
            pct: Some(45.0),
            done: Some(123),
            total: Some(2743),
            bytes: Some(1.2 * 1024.0 * 1024.0),
            rate: Some(3.4 * 1024.0 * 1024.0),
        };
        let line = ui.render(&s);
        assert!(line.contains("Receiving"), "{line}");
        assert!(line.contains("45%"), "{line}");
        assert!(line.contains("MiB"), "{line}");
        assert!(line.contains("ETA"), "{line}");
        // bar fill ≈ 45% of bar width
        let filled = line.matches('█').count();
        let empty = line.matches('░').count();
        let bar_w = filled + empty;
        assert_eq!(filled, (bar_w as f64 * 0.45).round() as usize, "{line}");
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("512"), Some(512.0));
        assert_eq!(parse_size("512 B"), Some(512.0));
        assert_eq!(parse_size("1.5 KiB"), Some(1.5 * 1024.0));
        assert_eq!(parse_size("2 GiB"), Some(2.0 * 1024.0 * 1024.0 * 1024.0));
        assert_eq!(parse_size("bogus"), None);
    }
}
