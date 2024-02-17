use jiffy::*;

#[test]
fn test_size_str_to_int() {
    assert_eq!(parse_size("1024b").unwrap(), 1024);
    assert_eq!(parse_size("1k").unwrap(), 1024);
    assert_eq!(parse_size("1.5M").unwrap(), (1024.0 * 1024.0 * 1.5) as u64);
    assert_eq!(parse_size("1.5").unwrap(), (1024.0 * 1024.0 * 1.5) as u64);
    assert_eq!(parse_size(".5").unwrap(), (1024.0 * 1024.0 * 0.5) as u64);
    assert_eq!(parse_size("2g").unwrap(), (1024.0 * 1024.0 * 1024.0 * 2.0) as u64);
    assert_eq!(parse_size("0.002T").unwrap(), (1024.0 * 1024.0 * 1024.0 * 1024.0 * 0.002) as u64);
}

#[test]
fn test_minimum_size_input() {
    fn input_too_small_wrapper(size: u64, size_str: &str) -> bool {
        input_too_small(size, &Some(size_str.to_string())).unwrap()
    }
    assert!(input_too_small_wrapper((2.5 * 1024.0 * 1024.0) as u64 - 1, "2.5M"));
    assert!(!input_too_small_wrapper((2.5 * 1024.0 * 1024.0) as u64, "2.5M"));
}
