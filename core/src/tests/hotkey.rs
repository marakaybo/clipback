use super::*;

#[test]
fn parses_combos() {
    assert_eq!(parse("Alt+F10").unwrap(), (MOD_ALT, 0x79));
    assert_eq!(parse("ctrl + shift + s").unwrap(), (MOD_CONTROL | MOD_SHIFT, b'S' as u32));
    assert_eq!(parse("Alt+Z").unwrap(), (MOD_ALT, b'Z' as u32));
    assert_eq!(parse("F24").unwrap().1, 0x87);
    assert_eq!(parse("Win+1").unwrap(), (MOD_WIN, b'1' as u32));
}

#[test]
fn rejects_bad_combos() {
    assert!(parse("Alt").is_err());
    assert!(parse("Alt+F25").is_err());
    assert!(parse("Alt+Пробел").is_err());
    assert!(parse("").is_err());
}
