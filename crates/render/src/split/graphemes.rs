//! Which characters a cut must not separate from the one before them.
//!
//! An approximation of Unicode's extended grapheme clusters (UAX #29) that
//! needs no dependency: marks and extenders attach to the character before
//! them, and so do a few pairs the rules name explicitly.

/// Whether a cut between `prev` and `c` would split a grapheme cluster:
/// `c` is a mark or other extender, `prev` is a zero-width joiner, `prev` is
/// an Indic virama that joins `c` into a conjunct, or both are parts of one
/// Hangul syllable. Regional indicator pairs are handled by the caller,
/// since they need a count.
pub(super) fn attaches(prev: char, c: char) -> bool {
    is_extender(c)
        || prev == '\u{200D}'
        || (is_linker(prev) && c.is_alphabetic())
        || (is_hangul_leading(prev) && (is_hangul_leading(c) || is_hangul_syllable(c)))
}

/// Whether `c` is a regional indicator, half of a flag.
pub(super) fn is_regional(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

fn is_extender(c: char) -> bool {
    EXTEND
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// The viramas Unicode 17 gives `Indic_Conjunct_Break=Linker`: a consonant
/// after one continues the cluster.
fn is_linker(c: char) -> bool {
    matches!(
        c,
        '\u{94D}'
            | '\u{9CD}'
            | '\u{ACD}'
            | '\u{B4D}'
            | '\u{C4D}'
            | '\u{D4D}'
            | '\u{1039}'
            | '\u{17D2}'
            | '\u{1A60}'
            | '\u{1B44}'
            | '\u{1BAB}'
            | '\u{A9C0}'
            | '\u{AAF6}'
            | '\u{10A3F}'
            | '\u{11133}'
            | '\u{113D0}'
            | '\u{1193E}'
            | '\u{11A47}'
            | '\u{11A99}'
            | '\u{11F42}'
    )
}

/// Hangul leading consonant jamo, `Grapheme_Cluster_Break=L`.
fn is_hangul_leading(c: char) -> bool {
    matches!(c, '\u{1100}'..='\u{115F}' | '\u{A960}'..='\u{A97C}')
}

fn is_hangul_syllable(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}')
}

/// Characters that attach to the one before them, as sorted, disjoint,
/// inclusive ranges: every character of general category `Mn`, `Mc` or
/// `Me` (nonspacing, spacing and enclosing marks), plus the characters
/// Unicode 17 gives `Grapheme_Cluster_Break` `Extend`, `SpacingMark`, `V`,
/// `T` or `ZWJ`. That covers combining accents, Hebrew and Arabic points,
/// Indic and Thai vowel signs, Hangul vowel and trailing jamo, variation
/// selectors, emoji modifiers, the zero-width joiner, the keycap mark and
/// emoji tag characters. Generated from the Unicode 17 tables of the
/// `unicode-segmentation` crate, merged with Python's `unicodedata`
/// categories.
#[rustfmt::skip]
const EXTEND: &[(char, char)] = &[
    ('\u{300}', '\u{36F}'), ('\u{483}', '\u{489}'), ('\u{591}', '\u{5BD}'), ('\u{5BF}', '\u{5BF}'),
    ('\u{5C1}', '\u{5C2}'), ('\u{5C4}', '\u{5C5}'), ('\u{5C7}', '\u{5C7}'), ('\u{610}', '\u{61A}'),
    ('\u{64B}', '\u{65F}'), ('\u{670}', '\u{670}'), ('\u{6D6}', '\u{6DC}'), ('\u{6DF}', '\u{6E4}'),
    ('\u{6E7}', '\u{6E8}'), ('\u{6EA}', '\u{6ED}'), ('\u{711}', '\u{711}'), ('\u{730}', '\u{74A}'),
    ('\u{7A6}', '\u{7B0}'), ('\u{7EB}', '\u{7F3}'), ('\u{7FD}', '\u{7FD}'), ('\u{816}', '\u{819}'),
    ('\u{81B}', '\u{823}'), ('\u{825}', '\u{827}'), ('\u{829}', '\u{82D}'), ('\u{859}', '\u{85B}'),
    ('\u{897}', '\u{89F}'), ('\u{8CA}', '\u{8E1}'), ('\u{8E3}', '\u{903}'), ('\u{93A}', '\u{93C}'),
    ('\u{93E}', '\u{94F}'), ('\u{951}', '\u{957}'), ('\u{962}', '\u{963}'), ('\u{981}', '\u{983}'),
    ('\u{9BC}', '\u{9BC}'), ('\u{9BE}', '\u{9C4}'), ('\u{9C7}', '\u{9C8}'), ('\u{9CB}', '\u{9CD}'),
    ('\u{9D7}', '\u{9D7}'), ('\u{9E2}', '\u{9E3}'), ('\u{9FE}', '\u{9FE}'), ('\u{A01}', '\u{A03}'),
    ('\u{A3C}', '\u{A3C}'), ('\u{A3E}', '\u{A42}'), ('\u{A47}', '\u{A48}'), ('\u{A4B}', '\u{A4D}'),
    ('\u{A51}', '\u{A51}'), ('\u{A70}', '\u{A71}'), ('\u{A75}', '\u{A75}'), ('\u{A81}', '\u{A83}'),
    ('\u{ABC}', '\u{ABC}'), ('\u{ABE}', '\u{AC5}'), ('\u{AC7}', '\u{AC9}'), ('\u{ACB}', '\u{ACD}'),
    ('\u{AE2}', '\u{AE3}'), ('\u{AFA}', '\u{AFF}'), ('\u{B01}', '\u{B03}'), ('\u{B3C}', '\u{B3C}'),
    ('\u{B3E}', '\u{B44}'), ('\u{B47}', '\u{B48}'), ('\u{B4B}', '\u{B4D}'), ('\u{B55}', '\u{B57}'),
    ('\u{B62}', '\u{B63}'), ('\u{B82}', '\u{B82}'), ('\u{BBE}', '\u{BC2}'), ('\u{BC6}', '\u{BC8}'),
    ('\u{BCA}', '\u{BCD}'), ('\u{BD7}', '\u{BD7}'), ('\u{C00}', '\u{C04}'), ('\u{C3C}', '\u{C3C}'),
    ('\u{C3E}', '\u{C44}'), ('\u{C46}', '\u{C48}'), ('\u{C4A}', '\u{C4D}'), ('\u{C55}', '\u{C56}'),
    ('\u{C62}', '\u{C63}'), ('\u{C81}', '\u{C83}'), ('\u{CBC}', '\u{CBC}'), ('\u{CBE}', '\u{CC4}'),
    ('\u{CC6}', '\u{CC8}'), ('\u{CCA}', '\u{CCD}'), ('\u{CD5}', '\u{CD6}'), ('\u{CE2}', '\u{CE3}'),
    ('\u{CF3}', '\u{CF3}'), ('\u{D00}', '\u{D03}'), ('\u{D3B}', '\u{D3C}'), ('\u{D3E}', '\u{D44}'),
    ('\u{D46}', '\u{D48}'), ('\u{D4A}', '\u{D4D}'), ('\u{D57}', '\u{D57}'), ('\u{D62}', '\u{D63}'),
    ('\u{D81}', '\u{D83}'), ('\u{DCA}', '\u{DCA}'), ('\u{DCF}', '\u{DD4}'), ('\u{DD6}', '\u{DD6}'),
    ('\u{DD8}', '\u{DDF}'), ('\u{DF2}', '\u{DF3}'), ('\u{E31}', '\u{E31}'), ('\u{E33}', '\u{E3A}'),
    ('\u{E47}', '\u{E4E}'), ('\u{EB1}', '\u{EB1}'), ('\u{EB3}', '\u{EBC}'), ('\u{EC8}', '\u{ECE}'),
    ('\u{F18}', '\u{F19}'), ('\u{F35}', '\u{F35}'), ('\u{F37}', '\u{F37}'), ('\u{F39}', '\u{F39}'),
    ('\u{F3E}', '\u{F3F}'), ('\u{F71}', '\u{F84}'), ('\u{F86}', '\u{F87}'), ('\u{F8D}', '\u{F97}'),
    ('\u{F99}', '\u{FBC}'), ('\u{FC6}', '\u{FC6}'), ('\u{102B}', '\u{103E}'),
    ('\u{1056}', '\u{1059}'), ('\u{105E}', '\u{1060}'), ('\u{1062}', '\u{1064}'),
    ('\u{1067}', '\u{106D}'), ('\u{1071}', '\u{1074}'), ('\u{1082}', '\u{108D}'),
    ('\u{108F}', '\u{108F}'), ('\u{109A}', '\u{109D}'), ('\u{1160}', '\u{11FF}'),
    ('\u{135D}', '\u{135F}'), ('\u{1712}', '\u{1715}'), ('\u{1732}', '\u{1734}'),
    ('\u{1752}', '\u{1753}'), ('\u{1772}', '\u{1773}'), ('\u{17B4}', '\u{17D3}'),
    ('\u{17DD}', '\u{17DD}'), ('\u{180B}', '\u{180D}'), ('\u{180F}', '\u{180F}'),
    ('\u{1885}', '\u{1886}'), ('\u{18A9}', '\u{18A9}'), ('\u{1920}', '\u{192B}'),
    ('\u{1930}', '\u{193B}'), ('\u{1A17}', '\u{1A1B}'), ('\u{1A55}', '\u{1A5E}'),
    ('\u{1A60}', '\u{1A7C}'), ('\u{1A7F}', '\u{1A7F}'), ('\u{1AB0}', '\u{1ADD}'),
    ('\u{1AE0}', '\u{1AEB}'), ('\u{1B00}', '\u{1B04}'), ('\u{1B34}', '\u{1B44}'),
    ('\u{1B6B}', '\u{1B73}'), ('\u{1B80}', '\u{1B82}'), ('\u{1BA1}', '\u{1BAD}'),
    ('\u{1BE6}', '\u{1BF3}'), ('\u{1C24}', '\u{1C37}'), ('\u{1CD0}', '\u{1CD2}'),
    ('\u{1CD4}', '\u{1CE8}'), ('\u{1CED}', '\u{1CED}'), ('\u{1CF4}', '\u{1CF4}'),
    ('\u{1CF7}', '\u{1CF9}'), ('\u{1DC0}', '\u{1DFF}'), ('\u{200C}', '\u{200D}'),
    ('\u{20D0}', '\u{20F0}'), ('\u{2CEF}', '\u{2CF1}'), ('\u{2D7F}', '\u{2D7F}'),
    ('\u{2DE0}', '\u{2DFF}'), ('\u{302A}', '\u{302F}'), ('\u{3099}', '\u{309A}'),
    ('\u{A66F}', '\u{A672}'), ('\u{A674}', '\u{A67D}'), ('\u{A69E}', '\u{A69F}'),
    ('\u{A6F0}', '\u{A6F1}'), ('\u{A802}', '\u{A802}'), ('\u{A806}', '\u{A806}'),
    ('\u{A80B}', '\u{A80B}'), ('\u{A823}', '\u{A827}'), ('\u{A82C}', '\u{A82C}'),
    ('\u{A880}', '\u{A881}'), ('\u{A8B4}', '\u{A8C5}'), ('\u{A8E0}', '\u{A8F1}'),
    ('\u{A8FF}', '\u{A8FF}'), ('\u{A926}', '\u{A92D}'), ('\u{A947}', '\u{A953}'),
    ('\u{A980}', '\u{A983}'), ('\u{A9B3}', '\u{A9C0}'), ('\u{A9E5}', '\u{A9E5}'),
    ('\u{AA29}', '\u{AA36}'), ('\u{AA43}', '\u{AA43}'), ('\u{AA4C}', '\u{AA4D}'),
    ('\u{AA7B}', '\u{AA7D}'), ('\u{AAB0}', '\u{AAB0}'), ('\u{AAB2}', '\u{AAB4}'),
    ('\u{AAB7}', '\u{AAB8}'), ('\u{AABE}', '\u{AABF}'), ('\u{AAC1}', '\u{AAC1}'),
    ('\u{AAEB}', '\u{AAEF}'), ('\u{AAF5}', '\u{AAF6}'), ('\u{ABE3}', '\u{ABEA}'),
    ('\u{ABEC}', '\u{ABED}'), ('\u{D7B0}', '\u{D7C6}'), ('\u{D7CB}', '\u{D7FB}'),
    ('\u{FB1E}', '\u{FB1E}'), ('\u{FE00}', '\u{FE0F}'), ('\u{FE20}', '\u{FE2F}'),
    ('\u{FF9E}', '\u{FF9F}'), ('\u{101FD}', '\u{101FD}'), ('\u{102E0}', '\u{102E0}'),
    ('\u{10376}', '\u{1037A}'), ('\u{10A01}', '\u{10A03}'), ('\u{10A05}', '\u{10A06}'),
    ('\u{10A0C}', '\u{10A0F}'), ('\u{10A38}', '\u{10A3A}'), ('\u{10A3F}', '\u{10A3F}'),
    ('\u{10AE5}', '\u{10AE6}'), ('\u{10D24}', '\u{10D27}'), ('\u{10D69}', '\u{10D6D}'),
    ('\u{10EAB}', '\u{10EAC}'), ('\u{10EFA}', '\u{10EFF}'), ('\u{10F46}', '\u{10F50}'),
    ('\u{10F82}', '\u{10F85}'), ('\u{11000}', '\u{11002}'), ('\u{11038}', '\u{11046}'),
    ('\u{11070}', '\u{11070}'), ('\u{11073}', '\u{11074}'), ('\u{1107F}', '\u{11082}'),
    ('\u{110B0}', '\u{110BA}'), ('\u{110C2}', '\u{110C2}'), ('\u{11100}', '\u{11102}'),
    ('\u{11127}', '\u{11134}'), ('\u{11145}', '\u{11146}'), ('\u{11173}', '\u{11173}'),
    ('\u{11180}', '\u{11182}'), ('\u{111B3}', '\u{111C0}'), ('\u{111C9}', '\u{111CC}'),
    ('\u{111CE}', '\u{111CF}'), ('\u{1122C}', '\u{11237}'), ('\u{1123E}', '\u{1123E}'),
    ('\u{11241}', '\u{11241}'), ('\u{112DF}', '\u{112EA}'), ('\u{11300}', '\u{11303}'),
    ('\u{1133B}', '\u{1133C}'), ('\u{1133E}', '\u{11344}'), ('\u{11347}', '\u{11348}'),
    ('\u{1134B}', '\u{1134D}'), ('\u{11357}', '\u{11357}'), ('\u{11362}', '\u{11363}'),
    ('\u{11366}', '\u{1136C}'), ('\u{11370}', '\u{11374}'), ('\u{113B8}', '\u{113C0}'),
    ('\u{113C2}', '\u{113C2}'), ('\u{113C5}', '\u{113C5}'), ('\u{113C7}', '\u{113CA}'),
    ('\u{113CC}', '\u{113D0}'), ('\u{113D2}', '\u{113D2}'), ('\u{113E1}', '\u{113E2}'),
    ('\u{11435}', '\u{11446}'), ('\u{1145E}', '\u{1145E}'), ('\u{114B0}', '\u{114C3}'),
    ('\u{115AF}', '\u{115B5}'), ('\u{115B8}', '\u{115C0}'), ('\u{115DC}', '\u{115DD}'),
    ('\u{11630}', '\u{11640}'), ('\u{116AB}', '\u{116B7}'), ('\u{1171D}', '\u{1172B}'),
    ('\u{1182C}', '\u{1183A}'), ('\u{11930}', '\u{11935}'), ('\u{11937}', '\u{11938}'),
    ('\u{1193B}', '\u{1193E}'), ('\u{11940}', '\u{11940}'), ('\u{11942}', '\u{11943}'),
    ('\u{119D1}', '\u{119D7}'), ('\u{119DA}', '\u{119E0}'), ('\u{119E4}', '\u{119E4}'),
    ('\u{11A01}', '\u{11A0A}'), ('\u{11A33}', '\u{11A39}'), ('\u{11A3B}', '\u{11A3E}'),
    ('\u{11A47}', '\u{11A47}'), ('\u{11A51}', '\u{11A5B}'), ('\u{11A8A}', '\u{11A99}'),
    ('\u{11B60}', '\u{11B67}'), ('\u{11C2F}', '\u{11C36}'), ('\u{11C38}', '\u{11C3F}'),
    ('\u{11C92}', '\u{11CA7}'), ('\u{11CA9}', '\u{11CB6}'), ('\u{11D31}', '\u{11D36}'),
    ('\u{11D3A}', '\u{11D3A}'), ('\u{11D3C}', '\u{11D3D}'), ('\u{11D3F}', '\u{11D45}'),
    ('\u{11D47}', '\u{11D47}'), ('\u{11D8A}', '\u{11D8E}'), ('\u{11D90}', '\u{11D91}'),
    ('\u{11D93}', '\u{11D97}'), ('\u{11EF3}', '\u{11EF6}'), ('\u{11F00}', '\u{11F01}'),
    ('\u{11F03}', '\u{11F03}'), ('\u{11F34}', '\u{11F3A}'), ('\u{11F3E}', '\u{11F42}'),
    ('\u{11F5A}', '\u{11F5A}'), ('\u{13440}', '\u{13440}'), ('\u{13447}', '\u{13455}'),
    ('\u{1611E}', '\u{1612F}'), ('\u{16AF0}', '\u{16AF4}'), ('\u{16B30}', '\u{16B36}'),
    ('\u{16D63}', '\u{16D63}'), ('\u{16D67}', '\u{16D6A}'), ('\u{16F4F}', '\u{16F4F}'),
    ('\u{16F51}', '\u{16F87}'), ('\u{16F8F}', '\u{16F92}'), ('\u{16FE4}', '\u{16FE4}'),
    ('\u{16FF0}', '\u{16FF1}'), ('\u{1BC9D}', '\u{1BC9E}'), ('\u{1CF00}', '\u{1CF2D}'),
    ('\u{1CF30}', '\u{1CF46}'), ('\u{1D165}', '\u{1D169}'), ('\u{1D16D}', '\u{1D172}'),
    ('\u{1D17B}', '\u{1D182}'), ('\u{1D185}', '\u{1D18B}'), ('\u{1D1AA}', '\u{1D1AD}'),
    ('\u{1D242}', '\u{1D244}'), ('\u{1DA00}', '\u{1DA36}'), ('\u{1DA3B}', '\u{1DA6C}'),
    ('\u{1DA75}', '\u{1DA75}'), ('\u{1DA84}', '\u{1DA84}'), ('\u{1DA9B}', '\u{1DA9F}'),
    ('\u{1DAA1}', '\u{1DAAF}'), ('\u{1E000}', '\u{1E006}'), ('\u{1E008}', '\u{1E018}'),
    ('\u{1E01B}', '\u{1E021}'), ('\u{1E023}', '\u{1E024}'), ('\u{1E026}', '\u{1E02A}'),
    ('\u{1E08F}', '\u{1E08F}'), ('\u{1E130}', '\u{1E136}'), ('\u{1E2AE}', '\u{1E2AE}'),
    ('\u{1E2EC}', '\u{1E2EF}'), ('\u{1E4EC}', '\u{1E4EF}'), ('\u{1E5EE}', '\u{1E5EF}'),
    ('\u{1E6E3}', '\u{1E6E3}'), ('\u{1E6E6}', '\u{1E6E6}'), ('\u{1E6EE}', '\u{1E6EF}'),
    ('\u{1E6F5}', '\u{1E6F5}'), ('\u{1E8D0}', '\u{1E8D6}'), ('\u{1E944}', '\u{1E94A}'),
    ('\u{1F3FB}', '\u{1F3FF}'), ('\u{E0020}', '\u{E007F}'), ('\u{E0100}', '\u{E01EF}'),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_and_disjoint() {
        assert!(EXTEND.iter().all(|(lo, hi)| lo <= hi));
        assert!(EXTEND.windows(2).all(|w| w[0].1 < w[1].0));
    }

    #[test]
    fn marks_attach() {
        for c in [
            '\u{301}',
            '\u{5B4}',
            '\u{64E}',
            '\u{93F}',
            '\u{94D}',
            '\u{E34}',
            '\u{E33}',
            '\u{1160}',
            '\u{11A8}',
            '\u{D7B0}',
            '\u{D7FB}',
            '\u{FE0F}',
            '\u{1F3FD}',
            '\u{20E3}',
            '\u{E0067}',
            '\u{200D}',
            '\u{102B}',
        ] {
            assert!(attaches('a', c), "{c:?}");
        }
        for c in [
            'a', ' ', '\n', 'क', '\u{AC00}', '😀', '\u{1100}', '\u{200B}',
        ] {
            assert!(!attaches('a', c), "{c:?}");
        }
    }

    #[test]
    fn pairs_attach() {
        assert!(attaches('\u{200D}', '👧'));
        assert!(attaches('\u{94D}', 'ष'));
        assert!(!attaches('\u{94D}', ' '));
        assert!(attaches('\u{1100}', '\u{1100}'));
        assert!(attaches('\u{1100}', '\u{AC00}'));
        assert!(!attaches('\u{AC00}', '\u{1100}'));
    }
}
