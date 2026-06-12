use std::os::raw::c_uint;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn log(message: &str) {
    println!("[stow] {message}");
}

pub fn set_umask() {
    unsafe {
        umask(0o077);
    }
}

pub fn unique_suffix() -> Option<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{nanos:x}"))
}

pub fn rand_suffix(len: usize) -> String {
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        seed ^= seed << 7;
        seed ^= seed >> 9;
        seed ^= seed << 8;
        let index = (seed as usize) % alphabet.len();
        out.push(alphabet[index] as char);
    }
    out
}

unsafe extern "C" {
    fn umask(mask: c_uint) -> c_uint;
}

#[cfg(test)]
mod tests {
    use super::{rand_suffix, unique_suffix};

    #[test]
    fn rand_suffix_has_requested_length_from_known_alphabet() {
        let suffix = rand_suffix(8);
        assert_eq!(suffix.len(), 8);
        assert!(suffix
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()));
        assert_eq!(rand_suffix(0), "");
    }

    #[test]
    fn unique_suffix_is_lowercase_hex() {
        let suffix = unique_suffix().unwrap();
        assert!(!suffix.is_empty());
        assert!(suffix.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
