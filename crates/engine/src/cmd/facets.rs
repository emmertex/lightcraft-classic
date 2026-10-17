//! `library.facets`: the distinct values of metadata fields among the photos in view, with how many photos have
//! each (issue #218: the first piece of Lightroom Classic's Metadata filter columns, and of a table view's column
//! filters). Each field is counted with its own constraint left out of the filter, so a field already narrowed to
//! one value still lists the others it could take; the other fields' constraints and the source apply.

use std::collections::HashMap;

use lcc_catalog::{Filter, Flag, MediaKind, Photo, Sort};
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, cmd};
use crate::{Result, Session};

/// The fields `library.facets` counts.
pub const FIELDS: [&str; 11] = ["camera", "lens", "keyword", "person", "label", "flag", "rating", "kind", "year", "iso", "focal"];
/// Values one field returns without a `limit`.
pub const LIMIT_DEFAULT: usize = 200;
/// Most values one field returns.
pub const LIMIT_MAX: usize = 2000;

/// `f` without the constraint of `field` (the ones the filter bar and sidebar set for it).
fn without(field: &str, f: &Filter) -> Filter {
    let mut f = f.clone();
    match field {
        "camera" => f.camera = None,
        "lens" => f.lens = None,
        "keyword" => f.keyword = None,
        "person" => f.person = None,
        "label" => {
            f.label = None;
            f.labels.clear();
        }
        "flag" => f.flag = None,
        "rating" => f.rating = 0,
        "kind" => f.kind = None,
        "year" => {
            f.date = None;
            f.date_from = None;
            f.date_to = None;
        }
        // ISO and focal length are asked only in the search text (`iso:>800`), which stays
        _ => {}
    }
    f
}

/// The values `field` takes for photo `p` (several for keywords and people; none when it has no value).
fn values(s: &Session, field: &str, p: &Photo) -> Vec<String> {
    let text = |t: &str| if t.trim().is_empty() { Vec::new() } else { vec![t.trim().to_string()] };
    match field {
        "camera" => text(&p.meta.camera),
        "lens" => text(&p.meta.lens),
        "keyword" => p.meta.keywords.iter().filter(|k| !k.trim().is_empty()).cloned().collect(),
        "person" => p.people().into_iter().map(str::to_string).collect(),
        "label" => p.label.map(|l| s.catalog.label_name(l)).into_iter().collect(),
        "flag" => vec![
            match p.flag {
                Flag::Pick => "pick",
                Flag::Reject => "reject",
                Flag::None => "none",
            }
            .to_string(),
        ],
        "rating" => vec![p.rating.to_string()],
        "kind" => vec![
            match p.kind {
                MediaKind::Image => "image",
                MediaKind::Raw => "raw",
                MediaKind::Video => "video",
            }
            .to_string(),
        ],
        "year" => {
            p.captured.as_deref().and_then(|c| c.get(..4)).filter(|y| y.chars().all(|c| c.is_ascii_digit())).map(str::to_string).into_iter().collect()
        }
        "iso" => p.meta.iso.map(|i| i.to_string()).into_iter().collect(),
        "focal" => p.meta.focal_mm.filter(|f| f.is_finite() && *f > 0.0).map(|f| format!("{}", (f * 10.0).round() / 10.0)).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Numeric fields sort by value; the rest by count, most first, then by name.
fn numeric(field: &str) -> bool {
    matches!(field, "rating" | "year" | "iso" | "focal")
}

fn facets(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "library.facets";
    let fields: Vec<String> = match p.get("fields") {
        None | Some(Value::Null) => FIELDS.iter().map(|f| f.to_string()).collect(),
        Some(Value::Array(a)) => {
            let mut out = Vec::new();
            for v in a {
                let Some(name) = v.as_str().filter(|n| FIELDS.contains(n)) else {
                    return Err(bad(C, format!("fields are among {}", FIELDS.join(", "))));
                };
                if !out.iter().any(|o| o == name) {
                    out.push(name.to_string());
                }
            }
            out
        }
        Some(_) => return Err(bad(C, "fields must be a list of field names")),
    };
    let limit = match p.get("limit") {
        None | Some(Value::Null) => LIMIT_DEFAULT,
        Some(v) => {
            v.as_u64().map(|n| usize::try_from(n).unwrap_or(usize::MAX).min(LIMIT_MAX)).ok_or_else(|| bad(C, "limit must be a whole number"))?
        }
    };
    let mut out = serde_json::Map::new();
    for field in &fields {
        let f = s.view_filter(s.source, &without(field, &s.filter));
        let ids = s.catalog.query(&f, &Sort::default());
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut none = 0usize;
        for photo in ids.iter().filter_map(|id| s.catalog.photo(*id)) {
            let mut vals = values(s, field, photo);
            vals.sort();
            vals.dedup();
            if vals.is_empty() {
                none += 1;
            }
            for v in vals {
                *counts.entry(v).or_default() += 1;
            }
        }
        let distinct = counts.len();
        let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
        if numeric(field) {
            rows.sort_by(|a, b| {
                a.0.parse::<f64>().unwrap_or(f64::MAX).total_cmp(&b.0.parse::<f64>().unwrap_or(f64::MAX)).then_with(|| a.0.cmp(&b.0))
            });
        } else {
            rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        }
        let shown: Vec<Value> = rows.into_iter().take(limit).map(|(value, count)| json!({"value": value, "count": count})).collect();
        out.insert(field.clone(), json!({"photos": ids.len(), "none": none, "distinct": distinct, "values": shown}));
    }
    Ok(Value::Object(out))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        query "library.facets",
        "Metadata Facets",
        [],
        None,
        "{fields?: [camera | lens | keyword | person | label | flag | rating | kind | year | iso | focal] (default: all), limit? (values per field, default 200, at most 2000)} → {<field>: {photos, none, distinct, values: [{value, count}]}} — the values each field takes among the photos in view (the source, and the filter without that field's own constraint), with how many photos have each; `none`: photos without one. Counts by most photos first; rating, year, ISO and focal length by value",
        always,
        facets
    )]
}
