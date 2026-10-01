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
        for c in [
            '\u{00AD}',
            '\u{034F}',
            '\u{115F}',
            '\u{17B5}',
            '\u{180F}',
            '\u{200B}',
            '\u{202E}',
            '\u{2065}',
            '\u{3164}',
            '\u{FE0F}',
            '\u{FEFF}',
            '\u{FFA0}',
            '\u{FFF8}',
            '\u{1BCA3}',
            '\u{1D17A}',
            '\u{E0001}',
            '\u{E0100}',
            '\u{E0FFF}',
        ] {
            assert!(is_default_ignorable(c), "{:X}", u32::from(c));
        }
        for c in [
            'a',
            ' ',
            '\u{00A0}',
            '\u{0300}',
            '\u{2028}',
            '\u{FFF9}',
            '\u{1F600}',
            '\u{E1000}',
        ] {
            assert!(!is_default_ignorable(c), "{:X}", u32::from(c));
        }
    }
}
