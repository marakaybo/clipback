use super::*;

#[test]
fn labels_from_window_titles() {
    assert_eq!(clip_label("Minecraft* 1.21.1 - Singleplayer").as_deref(), Some("Minecraft 1.21.1"));
    assert_eq!(clip_label("Battlefield™ 6").as_deref(), Some("Battlefield 6"));
    assert_eq!(clip_label("a:b/c\\d|e?f\"g<h>").as_deref(), Some("abcdefgh"));
    assert_eq!(clip_label("   ").as_deref(), None);
    assert_eq!(clip_label("Program Manager").as_deref(), None);
    assert_eq!(clip_label("Clipback").as_deref(), None);
    assert_eq!(clip_label(&"x".repeat(100)).unwrap().chars().count(), 40);
    assert_eq!(clip_label("name...").as_deref(), Some("name"));
}

#[test]
fn unique_paths_do_not_collide() {
    let dir = std::env::temp_dir().join("clipback-tests-unique");
    std::fs::create_dir_all(&dir).unwrap();
    let a = unique_path(&dir, "clip", "mp4");
    std::fs::write(&a, b"1").unwrap();
    let b = unique_path(&dir, "clip", "mp4");
    assert_ne!(a, b);
    assert!(b.to_string_lossy().ends_with("clip (2).mp4"));
    let _ = std::fs::remove_file(a);
}

#[test]
fn stamp_looks_like_a_date() {
    let s = file_stamp();
    assert_eq!(s.len(), 19);
    assert!(!s.contains(':'));
}
