//! `library.facets` (issue #218): the values each metadata field takes among the photos in view, with counts.

use lcc_catalog::Op;
use serde_json::json;

use crate::Session;

/// The demo library with three photos given a camera and keywords of their own (the others keep theirs).
fn library() -> (Session, Vec<lcc_catalog::PhotoId>) {
    let mut s = Session::with_demo();
    let ids: Vec<_> = s.catalog.photos().filter(|p| p.in_library()).take(3).map(|p| p.id).collect();
    for (id, camera, keywords) in
        [(ids[0], "Facet Cam A", vec!["qq-one"]), (ids[1], "Facet Cam A", vec!["qq-one", "qq-two"]), (ids[2], "Facet Cam B", vec!["qq-two"])]
    {
        let mut m = s.catalog.photo(id).unwrap().meta.clone();
        m.camera = camera.into();
        m.keywords = keywords.into_iter().map(String::from).collect();
        s.catalog.apply(Op::SetMeta { id, meta: Box::new(m) }).unwrap();
    }
    (s, ids)
}

fn values(r: &serde_json::Value, field: &str) -> Vec<(String, u64)> {
    r[field]["values"].as_array().unwrap().iter().map(|v| (v["value"].as_str().unwrap().to_string(), v["count"].as_u64().unwrap())).collect()
}

#[test]
fn counts_the_values_of_the_photos_in_view() {
    let (mut s, _) = library();
    let shown = s.visible().len() as u64;
    let r = s.execute("library.facets", &json!({"fields": ["camera", "keyword", "rating"]})).unwrap();
    assert_eq!(r["camera"]["photos"], shown);
    let cams = values(&r, "camera");
    assert!(cams.contains(&("Facet Cam A".to_string(), 2)) && cams.contains(&("Facet Cam B".to_string(), 1)), "{cams:?}");
    let with_camera: u64 = cams.iter().map(|c| c.1).sum();
    assert_eq!(with_camera + r["camera"]["none"].as_u64().unwrap(), shown, "every photo once, with a camera or without");
    assert_eq!(r["camera"]["distinct"].as_u64().unwrap() as usize, cams.len());
    // a photo with several keywords counts once under each
    let kws = values(&r, "keyword");
    assert!(kws.contains(&("qq-one".to_string(), 2)) && kws.contains(&("qq-two".to_string(), 2)), "{kws:?}");
    // most photos first; rating by value
    assert!(cams.windows(2).all(|w| w[0].1 >= w[1].1));
    let ratings: Vec<u32> = values(&r, "rating").iter().map(|v| v.0.parse().unwrap()).collect();
    assert!(ratings.windows(2).all(|w| w[0] < w[1]), "{ratings:?}");
    assert!(r.get("lens").is_none(), "only the fields asked for");
}

/// A field narrowed by the filter still lists the values it could take; the other fields count only what matches.
#[test]
fn a_fields_own_constraint_is_left_out() {
    let (mut s, _) = library();
    s.execute("library.filter", &json!({"camera": "Facet Cam A"})).unwrap();
    let r = s.execute("library.facets", &json!({"fields": ["camera", "keyword"]})).unwrap();
    let cams = values(&r, "camera");
    assert!(cams.iter().any(|c| c.0 == "Facet Cam B"), "the other camera is still offered: {cams:?}");
    assert_eq!(r["keyword"]["photos"], 2, "keywords of the two Facet Cam A photos");
    assert_eq!(values(&r, "keyword"), vec![("qq-one".to_string(), 2), ("qq-two".to_string(), 1)]);
}

#[test]
fn rejects_unknown_fields_and_bounds_the_values() {
    let (mut s, _) = library();
    assert!(s.execute("library.facets", &json!({"fields": ["camera", "shoe size"]})).is_err());
    assert!(s.execute("library.facets", &json!({"fields": "camera"})).is_err());
    assert!(s.execute("library.facets", &json!({"limit": -1})).is_err());
    let r = s.execute("library.facets", &json!({"fields": ["keyword"], "limit": 1})).unwrap();
    assert_eq!(r["keyword"]["values"].as_array().unwrap().len(), 1);
    assert!(r["keyword"]["distinct"].as_u64().unwrap() > 1, "distinct counts them all");
    let all = s.execute("library.facets", &json!({})).unwrap();
    assert_eq!(all.as_object().unwrap().len(), crate::cmd::facets::FIELDS.len());
}
