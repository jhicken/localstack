//! Terminal formatting helpers shared by the CLI commands.
use std::io::IsTerminal;
use std::sync::OnceLock;

pub fn use_color() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn wrap(open: u8, close: u8, s: &str) -> String {
    if use_color() {
        format!("\x1b[{open}m{s}\x1b[{close}m")
    } else {
        s.to_string()
    }
}
pub fn bold(s: &str) -> String { wrap(1, 22, s) }
pub fn dim(s: &str) -> String { wrap(2, 22, s) }
pub fn red(s: &str) -> String { wrap(31, 39, s) }
pub fn green(s: &str) -> String { wrap(32, 39, s) }
pub fn yellow(s: &str) -> String { wrap(33, 39, s) }
pub fn blue(s: &str) -> String { wrap(34, 39, s) }
pub fn magenta(s: &str) -> String { wrap(35, 39, s) }
pub fn cyan(s: &str) -> String { wrap(36, 39, s) }
pub fn gray(s: &str) -> String { wrap(90, 39, s) }

pub fn status_color(status: &str, s: &str) -> String {
    match status {
        "running" => green(s),
        "starting" | "stopping" => yellow(s),
        "crashed" | "errored" => red(s),
        _ => gray(s),
    }
}
pub fn status_dot(status: &str) -> String {
    status_color(status, "●")
}
pub fn health_label(h: &str) -> String {
    match h {
        "healthy" => green("healthy"),
        "unhealthy" => red("unhealthy"),
        "unknown" => yellow("checking"),
        _ => gray("—"),
    }
}

pub fn bytes(n: Option<f64>) -> String {
    let Some(mut n) = n.filter(|v| v.is_finite()) else { return "—".into() };
    let u = ["B", "KB", "MB", "GB", "TB"];
    let mut i = 0;
    while n >= 1024.0 && i < u.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if n < 10.0 && i > 0 {
        format!("{n:.1}{}", u[i])
    } else {
        format!("{n:.0}{}", u[i])
    }
}
pub fn rate(n: Option<f64>) -> String {
    match n {
        Some(_) => format!("{}/s", bytes(n)),
        None => "—".into(),
    }
}
pub fn pct(n: Option<f64>) -> String {
    n.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "—".into())
}
pub fn duration(ms: Option<i64>) -> String {
    let Some(ms) = ms.filter(|m| *m > 0) else { return "—".into() };
    let s = ms / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m {}s", s % 60);
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h {}m", m % 60);
    }
    format!("{}d {}h", h / 24, h % 24)
}

const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn resample(values: &[f64], width: usize) -> Vec<f64> {
    if values.len() <= width {
        return values.to_vec();
    }
    let step = values.len() as f64 / width as f64;
    (0..width)
        .map(|i| {
            let a = (i as f64 * step) as usize;
            let b = (((i + 1) as f64 * step) as usize).max(a + 1).min(values.len());
            values[a..b].iter().sum::<f64>() / (b - a) as f64
        })
        .collect()
}

pub fn sparkline(values: &[f64], width: usize) -> String {
    if values.is_empty() {
        return " ".repeat(width);
    }
    let vals = resample(values, width);
    let top = vals.iter().cloned().fold(1e-9, f64::max);
    let s: String = vals
        .iter()
        .map(|v| SPARK[((v / top) * 7.0).round().clamp(0.0, 7.0) as usize])
        .collect();
    format!("{s:>width$}")
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
pub fn vis_len(s: &str) -> usize {
    strip_ansi(s).chars().count()
}
pub fn truncate(s: &str, w: usize) -> String {
    if vis_len(s) <= w {
        return s.to_string();
    }
    let plain: String = strip_ansi(s).chars().take(w.saturating_sub(1)).collect();
    format!("{plain}…")
}
pub fn pad(s: &str, w: usize) -> String {
    let len = vis_len(s);
    if len >= w {
        truncate(s, w)
    } else {
        format!("{s}{}", " ".repeat(w - len))
    }
}
pub fn table(rows: &[Vec<String>], headers: &[&str]) -> String {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| rows.iter().map(|r| r.get(i).map(|c| vis_len(c)).unwrap_or(0)).max().unwrap_or(0).max(vis_len(h)))
        .collect();
    let line = |r: Vec<String>| r.iter().enumerate().map(|(i, c)| pad(c, widths[i])).collect::<Vec<_>>().join("  ");
    let mut out = vec![bold(&line(headers.iter().map(|h| h.to_string()).collect()))];
    out.extend(rows.iter().map(|r| line(r.clone())));
    out.join("\n")
}

pub fn local_time(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formatting() {
        assert_eq!(bytes(Some(512.0)), "512B");
        assert_eq!(bytes(Some(1536.0)), "1.5KB");
        assert_eq!(bytes(Some(50.0 * 1024.0 * 1024.0)), "50MB");
        assert_eq!(bytes(None), "—");
        assert_eq!(pct(Some(12.345)), "12.3%");
        assert_eq!(duration(Some(59_000)), "59s");
        assert_eq!(duration(Some(3_661_000)), "1h 1m");
        assert_eq!(duration(Some(90_000_000)), "1d 1h");
        assert_eq!(duration(Some(0)), "—");
        assert_eq!(sparkline(&[0.0, 1.0], 4), "  ▁█");
        assert_eq!(sparkline(&[1.0; 10], 5).chars().count(), 5);
        assert_eq!(vis_len("\x1b[31mabc\x1b[39m"), 3);
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(pad("ab", 4), "ab  ");
    }
}
