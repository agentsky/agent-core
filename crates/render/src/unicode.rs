//! Unicode properties the renderers and their callers share.

/// Whether `c` is one of Unicode's default-ignorable code points
/// (`Default_Ignorable_Code_Point` in `DerivedCoreProperties.txt`): the
/// characters that render as nothing, such as zero-width spaces, joiners,
/// bidirectional controls, variation selectors, Hangul fillers and tag
/// characters. IDNA drops them from hostnames, and they can hide text
/// from a reader that a model still reads.
pub fn is_default_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_ignorable_set_is_whole_and_no_more() {
        let ranges: [(u32, u32); 17] = [
            (0x00AD, 0x00AD),
            (0x034F, 0x034F),
            (0x061C, 0x061C),
            (0x115F, 0x1160),
            (0x17B4, 0x17B5),
            (0x180B, 0x180F),
            (0x200B, 0x200F),
            (0x202A, 0x202E),
            (0x2060, 0x206F),
            (0x3164, 0x3164),
            (0xFE00, 0xFE0F),
            (0xFEFF, 0xFEFF),
            (0xFFA0, 0xFFA0),
            (0xFFF0, 0xFFF8),
            (0x1BCA0, 0x1BCA3),
            (0x1D173, 0x1D17A),
            (0xE0000, 0xE0FFF),
        ];
        let char_at = |code: u32| char::from_u32(code).unwrap();
        for (first, last) in ranges {
            for code in [first, (first + last) / 2, last] {
                assert!(is_default_ignorable(char_at(code)), "{code:X}");
            }
            for code in [first - 1, last + 1] {
                assert!(!is_default_ignorable(char_at(code)), "{code:X}");
            }
        }
        for c in ['a', ' ', '\u{00A0}', '\u{0300}', '\u{2800}', '\u{1F600}'] {
            assert!(!is_default_ignorable(c), "{:X}", u32::from(c));
        }
    }
}
