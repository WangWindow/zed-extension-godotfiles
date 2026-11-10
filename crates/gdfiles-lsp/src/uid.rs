//! Godot resource UID text decoding.
//!
//! Godot encodes UIDs as `uid://` followed by base-34 digits. The legacy
//! alphabet is `a`-`y` for 0-24 and `0`-`8` for 25-33; `z` and `9` are not
//! emitted by Godot but are accepted by its decoder (and therefore by this
//! one). IDs wrap as unsigned 64-bit values and are masked to 63 bits.

/// Mask used by Godot (`ResourceUID` IDs have their high bit clear).
const UID_MASK: u64 = 0x7fff_ffff_ffff_ffff;

/// Decode a Godot UID text value (`uid://...`) to its numeric ID.
///
/// Returns `None` for values without the `uid://` prefix or containing
/// characters outside Godot's base-34 alphabet.
pub fn uid_to_id(value: &str) -> Option<u64> {
    let digits = value.strip_prefix("uid://")?;
    let mut uid: u64 = 0;
    for ch in digits.chars() {
        let digit = match ch {
            'a'..='z' => (ch as u64) - ('a' as u64),
            '0'..='9' => (ch as u64) - ('0' as u64) + 25,
            _ => return None,
        };
        uid = uid.wrapping_mul(34).wrapping_add(digit);
    }
    Some(uid & UID_MASK)
}
