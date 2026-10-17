//! `photo.sourceReport` (roadmap RQ-01): how originals are read before any edit, through the command layer.

use serde_json::json;

use crate::Session;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("lc-source-report-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn reports_how_each_original_is_read() {
    let dir = temp_dir("files");
    let dng = dir.join("IMG_0.dng");
    std::fs::write(&dng, lcc_merge::synth::bracket_dngs(96, 64, &[0.0]).unwrap().remove(0)).unwrap();
    let png = dir.join("plain.jpg");
    let img = lcc_raster::Rgba8::from_fn(40, 30, |x, y| [(x * 6) as u8, (y * 8) as u8, 90, 255]);
    let meta = lcc_codecs::EncodeMeta::default();
    std::fs::write(&png, lcc_codecs::encode_jpeg(&lcc_codecs::EncodeImage::rgba8(&img), 90, Default::default(), &meta).unwrap()).unwrap();
    let mut s = Session::new().with_fs();
    let r = s.execute("library.import", &json!({"paths": [dng, png]})).unwrap();
    let ids: Vec<u64> = r["imported"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    assert_eq!(ids.len(), 2);
    let out = s.execute("photo.sourceReport", &json!({"ids": ids})).unwrap();
    let photos = out["photos"].as_array().unwrap();
    let raw = &photos[0]["report"];
    assert_eq!(raw["raw"], true, "{out}");
    assert_eq!(raw["decode"]["kind"], "sensor");
    assert_eq!(raw["colour"]["kind"], "file", "a DNG's own matrices: {raw}");
    assert_eq!(raw["matrixIsFallback"], false);
    assert_eq!(raw["lens"]["source"], "none");
    let plain = &photos[1]["report"];
    assert_eq!((plain["raw"].clone(), plain["decode"]["kind"].clone()), (json!(false), json!("image")), "{plain}");
    // the active photo by default; nothing is written
    s.execute("library.select", &json!({"ids": [ids[0]]})).unwrap();
    let one = s.execute("photo.sourceReport", &json!({})).unwrap();
    assert_eq!(one["photos"].as_array().unwrap().len(), 1);
    assert_eq!(one["photos"][0]["id"], ids[0]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reports_what_it_cannot_read_per_photo() {
    let mut s = Session::with_demo();
    let id = s.catalog.photos().next().unwrap().id.0;
    let out = s.execute("photo.sourceReport", &json!({"ids": [id, 999_999]})).unwrap();
    assert_eq!(out["photos"][0]["error"], "a demo photo has no file", "{out}");
    assert_eq!(out["photos"][1]["error"], "no such photo");
    let many: Vec<u64> = (0..=crate::cmd::source::MAX_PHOTOS as u64).collect();
    assert!(s.execute("photo.sourceReport", &json!({"ids": many})).is_err(), "a bounded batch");
    assert!(s.execute("photo.sourceReport", &json!({"ids": "x"})).is_err());
}
