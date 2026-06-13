//! Reference solution for the dedup component. See dedup/SPEC.md.
//! Excluded from agent visibility by the SKILL runner.
//!
//! Streaming: one pass over the input lines, per-group running state in a
//! BTreeMap (winner, backfill-email candidate, tag union). Fits the 500k-line
//! / 10s performance budget with plenty of room.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, Write};

use serde_json::{Map, Value};

struct Group {
    /// Winner ordering key: (date, has_email, position) — larger wins.
    win_key: (String, bool, usize),
    winner: Map<String, Value>,
    /// Best (date, position) among members with a non-null email.
    backfill_key: Option<(String, usize)>,
    backfill_email: Option<String>,
    tags: BTreeSet<String>,
}

fn usage() -> ! {
    eprintln!("usage: dedup [--since YYYY-MM-DD] FILE1.jsonl [FILE2.jsonl ...]");
    std::process::exit(2);
}

fn is_iso_shape(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

/// Schema validation per dedup/SPEC.md "Record validation": non-conforming
/// records are skipped silently.
fn conforms(obj: &Map<String, Value>) -> bool {
    let id_ok = obj.get("id").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    let name_ok = obj.get("name").is_some_and(Value::is_string);
    let email_ok = obj
        .get("email")
        .is_some_and(|v| v.is_string() || v.is_null());
    let amount_ok = obj.get("amount").is_some_and(Value::is_number);
    let date_ok = obj
        .get("date")
        .and_then(Value::as_str)
        .is_some_and(is_iso_shape);
    let tags_ok = obj
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().all(Value::is_string));
    id_ok && name_ok && email_ok && amount_ok && date_ok && tags_ok
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut since: Option<String> = None;
    if args.first().map(String::as_str) == Some("--since") {
        if args.len() < 2 {
            usage();
        }
        let date = args[1].clone();
        if !is_iso_shape(&date) {
            usage();
        }
        since = Some(date);
        args.drain(0..2);
    }
    if args.is_empty() {
        usage();
    }

    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    let mut pos = 0usize;

    for path in &args {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("cannot read {}: {}", path, e);
                std::process::exit(1);
            }
        };
        for line in io::BufReader::new(file).lines() {
            let line = line.unwrap_or_default();
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("bad json: {}", e);
                    std::process::exit(1);
                }
            };
            let obj = match v {
                Value::Object(o) => o,
                _ => {
                    eprintln!("line is not a JSON object");
                    std::process::exit(1);
                }
            };
            if !conforms(&obj) {
                continue; // malformed record: skipped, no position
            }
            let date = obj["date"].as_str().unwrap_or_default().to_string();
            if let Some(s) = &since {
                if date.as_str() < s.as_str() {
                    continue; // filtered: no position, no tags, no candidacy
                }
            }
            let id = obj["id"].as_str().unwrap_or_default().to_string();
            let email = obj["email"].as_str().map(str::to_string);
            let row_tags: Vec<String> = obj["tags"]
                .as_array()
                .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
                .unwrap_or_default();

            let cur_pos = pos;
            pos += 1;
            let key = (date.clone(), email.is_some(), cur_pos);

            match groups.get_mut(&id) {
                Some(g) => {
                    if key > g.win_key {
                        g.win_key = key;
                        g.winner = obj;
                    }
                    if let Some(em) = &email {
                        let bk = (date.clone(), cur_pos);
                        if g.backfill_key.as_ref().map_or(true, |old| bk > *old) {
                            g.backfill_key = Some(bk);
                            g.backfill_email = Some(em.clone());
                        }
                    }
                    g.tags.extend(row_tags);
                }
                None => {
                    let backfill_key = email.as_ref().map(|_| (date.clone(), cur_pos));
                    groups.insert(
                        id,
                        Group {
                            win_key: key,
                            winner: obj,
                            backfill_key,
                            backfill_email: email,
                            tags: row_tags.into_iter().collect(),
                        },
                    );
                }
            }
        }
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for (_id, g) in groups {
        let mut rec = g.winner;
        rec.insert(
            "tags".into(),
            Value::Array(g.tags.into_iter().map(Value::String).collect()),
        );
        if rec.get("email").map_or(false, Value::is_null) {
            if let Some(em) = g.backfill_email {
                rec.insert("email".into(), Value::String(em));
            }
        }
        let line = serde_json::to_string(&Value::Object(rec)).unwrap();
        writeln!(out, "{}", line).unwrap();
    }
}
