//! Append-only JSONL metering store (H5 in DESIGN.md).
//!
//! One line per completed request. The format is crash-safe by construction: a
//! torn final line simply fails to parse and is skipped on read, so a daemon
//! killed mid-write never corrupts history. No database, no C deps — a plain
//! text log you can `cat`, `grep`, or ship.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Unix seconds when the request completed.
    pub ts: u64,
    /// "local" for the metering proxy, or a peer's short node id for mesh work.
    pub source: String,
    /// Direction from this machine's view: "served" (we ran it) or "used" (we
    /// ran it elsewhere / locally for ourselves).
    pub kind: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub duration_ms: u64,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn epoch_day(ts: u64) -> u64 {
    ts / 86_400
}

/// Append a record to today's log. Best-effort: metering must never break serving.
pub fn append(rec: &UsageRecord) {
    if let Err(e) = try_append(rec) {
        eprintln!("v2: usage log write failed: {e}");
    }
}

fn try_append(rec: &UsageRecord) -> std::io::Result<()> {
    let dir = paths::subdir("usage")?;
    let path = dir.join(format!("day-{}.jsonl", epoch_day(rec.ts)));
    let mut line = serde_json::to_string(rec).unwrap_or_default();
    line.push('\n');
    paths::append_private(&path, line.as_bytes())
}

/// Read every record across all daily logs, skipping torn/invalid lines.
pub fn read_all() -> Vec<UsageRecord> {
    let mut out = vec![];
    let Ok(dir) = paths::subdir("usage") else { return out };
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    let mut files: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // Torn-line-safe: a partially written final line just won't parse.
            if let Ok(rec) = serde_json::from_str::<UsageRecord>(line) {
                out.push(rec);
            }
        }
    }
    out
}

/// Convert a Unix day-count to a YYYY-MM-DD string (proleptic Gregorian).
/// Howard Hinnant's civil-from-days algorithm — pure, no date crate.
pub fn day_to_date(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

#[derive(Default, Clone)]
struct Agg {
    tokens_in: u64,
    tokens_out: u64,
    requests: u64,
    duration_ms: u64,
}

/// One grouped row (a day, a model, or a source) — pure data shared by the
/// CLI's printed table and the desktop app's usage panel.
#[derive(Debug, Clone, Serialize)]
pub struct AggRow {
    pub key: String,
    pub requests: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tps: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSummary {
    pub total_requests: u64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub by_day: Vec<AggRow>,
    pub by_model: Vec<AggRow>,
    pub by_source: Vec<AggRow>,
}

fn agg_rows<I: Iterator<Item = (String, Agg)>>(rows: I) -> Vec<AggRow> {
    let mut rows: Vec<_> = rows.collect();
    rows.sort_by(|a, b| (b.1.tokens_out).cmp(&a.1.tokens_out));
    rows.into_iter()
        .take(12)
        .map(|(key, a)| AggRow {
            key,
            requests: a.requests,
            tokens_in: a.tokens_in,
            tokens_out: a.tokens_out,
            tps: if a.duration_ms > 0 {
                a.tokens_out as f64 / (a.duration_ms as f64 / 1000.0)
            } else {
                0.0
            },
        })
        .collect()
}

/// Group records by day/model/source — no I/O, no printing.
pub fn summarize(records: &[UsageRecord]) -> UsageSummary {
    let mut by_day: BTreeMap<u64, Agg> = BTreeMap::new();
    let mut by_model: BTreeMap<String, Agg> = BTreeMap::new();
    let mut by_source: BTreeMap<String, Agg> = BTreeMap::new();
    let mut total = Agg::default();

    for r in records {
        for (map, key) in [
            (&mut by_model, r.model.clone()),
            (&mut by_source, r.source.clone()),
        ] {
            let a = map.entry(key).or_default();
            a.tokens_in += r.tokens_in;
            a.tokens_out += r.tokens_out;
            a.requests += 1;
            a.duration_ms += r.duration_ms;
        }
        let a = by_day.entry(epoch_day(r.ts)).or_default();
        a.tokens_in += r.tokens_in;
        a.tokens_out += r.tokens_out;
        a.requests += 1;
        a.duration_ms += r.duration_ms;

        total.tokens_in += r.tokens_in;
        total.tokens_out += r.tokens_out;
        total.requests += 1;
        total.duration_ms += r.duration_ms;
    }

    UsageSummary {
        total_requests: total.requests,
        total_tokens_in: total.tokens_in,
        total_tokens_out: total.tokens_out,
        by_day: agg_rows(by_day.into_iter().map(|(d, a)| (day_to_date(d * 86_400), a))),
        by_model: agg_rows(by_model.into_iter()),
        by_source: agg_rows(by_source.into_iter()),
    }
}

/// Print a compact usage summary grouped by day, then model, then source.
pub fn print_summary(records: &[UsageRecord], json: bool) {
    if json {
        let arr: Vec<_> = records
            .iter()
            .map(|r| serde_json::to_value(r).unwrap_or_default())
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
        return;
    }

    if records.is_empty() {
        crate::ui::section("usage");
        println!("  no records yet — run `v2 serve` and route apps through :11435 to meter");
        return;
    }

    let summary = summarize(records);

    crate::ui::panel(
        "usage",
        &[
            ("requests".into(), summary.total_requests.to_string()),
            ("tokens in".into(), fmt_tokens(summary.total_tokens_in)),
            ("tokens out".into(), fmt_tokens(summary.total_tokens_out)),
        ],
    );

    print_group("by day", &summary.by_day);
    print_group("by model", &summary.by_model);
    print_group("by source", &summary.by_source);
}

fn print_group(title: &str, rows: &[AggRow]) {
    crate::ui::section(title);
    for row in rows {
        println!(
            "    {:<20}  {:>4} req  {:>8} in  {:>8} out  {:>6.0} tok/s",
            row.key,
            row.requests,
            fmt_tokens(row.tokens_in),
            fmt_tokens(row.tokens_out),
            row.tps,
        );
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_conversion_known_epoch() {
        assert_eq!(day_to_date(0), "1970-01-01");
        // 2021-01-01 = 18628 days after epoch
        assert_eq!(day_to_date(18628 * 86_400), "2021-01-01");
    }

    #[test]
    fn torn_line_is_skipped() {
        let good = UsageRecord {
            ts: 100, source: "local".into(), kind: "served".into(),
            model: "qwen3:8b".into(), tokens_in: 10, tokens_out: 20, duration_ms: 500,
        };
        let line = serde_json::to_string(&good).unwrap();
        // A torn line (truncated JSON) must not parse.
        let torn = &line[..line.len() / 2];
        assert!(serde_json::from_str::<UsageRecord>(torn).is_err());
        assert!(serde_json::from_str::<UsageRecord>(&line).is_ok());
    }

    fn rec(ts: u64, source: &str, model: &str, tokens_in: u64, tokens_out: u64, duration_ms: u64) -> UsageRecord {
        UsageRecord { ts, source: source.into(), kind: "served".into(), model: model.into(), tokens_in, tokens_out, duration_ms }
    }

    // `summarize` is the data function behind both `v2 usage` and the desktop
    // app's usage panel — this is the "API test" for that shared surface.
    #[test]
    fn summarize_totals_and_groups_match_input() {
        let records = vec![
            rec(0, "local", "qwen3:8b", 10, 100, 1000),
            rec(1, "local", "qwen3:8b", 5, 50, 500),
            rec(86_400, "peer-a", "llama3.1", 20, 200, 2000),
        ];

        let s = summarize(&records);

        assert_eq!(s.total_requests, 3);
        assert_eq!(s.total_tokens_in, 35);
        assert_eq!(s.total_tokens_out, 350);

        // Two distinct days.
        assert_eq!(s.by_day.len(), 2);
        assert_eq!(s.by_day.iter().map(|r| r.requests).sum::<u64>(), 3);

        // by_model is sorted by tokens_out desc: llama3.1 (200) before qwen3:8b (150 combined).
        assert_eq!(s.by_model[0].key, "llama3.1");
        assert_eq!(s.by_model[0].tokens_out, 200);
        assert_eq!(s.by_model[1].key, "qwen3:8b");
        assert_eq!(s.by_model[1].requests, 2);
        assert_eq!(s.by_model[1].tokens_out, 150);

        assert_eq!(s.by_source.len(), 2);
        let local = s.by_source.iter().find(|r| r.key == "local").unwrap();
        assert_eq!(local.requests, 2);
        assert_eq!(local.tokens_out, 150);
        // tps = tokens_out / (duration_ms / 1000) summed: 150 tok / 1.5s = 100 tok/s.
        assert!((local.tps - 100.0).abs() < 0.01);
    }

    #[test]
    fn summarize_empty_is_all_zero() {
        let s = summarize(&[]);
        assert_eq!(s.total_requests, 0);
        assert!(s.by_day.is_empty());
        assert!(s.by_model.is_empty());
        assert!(s.by_source.is_empty());
    }
}
