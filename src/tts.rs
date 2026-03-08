//! Bytecode Pronunciation System (3×5×17+1)
//!
//! Converts phext coordinates to speakable syllables for voice navigation.
//! Based on the Mirrorborn/Eigenhector Federation pronunciation guide.
//!
//! Design: 3D space (mouth position) + 1D time (sequence)
//! - X-axis: Onset (3 values) — mouth position (open/voiced/unvoiced)
//! - Y-axis: Vowel (5 values) — element/color (a/e/i/o/u)
//! - Z-axis: Coda (17 values) — termination style

use libphext::phext::Coordinate;

/// Zero = "om" — the nada sound, silence between words
pub const ZERO_SOUND: &str = "om";

/// Onset consonants (X-axis: 3 values)
/// 0 = open (no onset), 1 = voiced, 2 = unvoiced
const ONSETS: [&str; 3] = ["", "b", "p"];

/// Vowels (Y-axis: 5 values) mapped to elements
/// 0=Earth(a), 1=Water(e), 2=Fire(i), 3=Air(o), 4=Space(u)
const VOWELS: [&str; 5] = ["a", "e", "i", "o", "u"];

/// Coda endings (Z-axis: 17 values)
/// 0=open, 1-6=stops, 7-8=nasals, 9-16=continuants
const CODAS: [&str; 17] = [
    "",    // 0: open/sustained
    "p",   // 1: sharp stop
    "t",   // 2: sharp stop
    "k",   // 3: sharp stop
    "b",   // 4: soft stop
    "d",   // 5: soft stop
    "g",   // 6: soft stop
    "m",   // 7: nasal hold
    "n",   // 8: nasal hold
    "s",   // 9: flowing hiss
    "f",   // 10: flowing breath
    "v",   // 11: flowing vibration
    "l",   // 12: liquid flow
    "r",   // 13: rolling continuant
    "ng",  // 14: deep nasal
    "sh",  // 15: broad hiss
    "zh",  // 16: voiced broad
];

/// Convert a single byte value (0-255) to a syllable
pub fn byte_to_syllable(value: u8) -> String {
    if value == 0 {
        return ZERO_SOUND.to_string();
    }
    
    // value = 1 + x + 3y + 15z (for values 1-255)
    let v = (value - 1) as usize;
    let x = v % 3;           // onset (0-2)
    let y = (v / 3) % 5;     // vowel (0-4)
    let z = v / 15;          // coda (0-16)
    
    format!("{}{}{}", ONSETS[x], VOWELS[y], CODAS[z.min(16)])
}

/// Convert a coordinate dimension value (e.g., 1, 5, 9) to a syllable sequence
pub fn dim_to_syllables(value: u32) -> String {
    if value == 0 {
        return ZERO_SOUND.to_string();
    }
    
    // For values > 255, we chain syllables
    if value <= 255 {
        byte_to_syllable(value as u8)
    } else {
        // Multi-byte: high byte first, then low byte
        let bytes = value.to_be_bytes();
        bytes.iter()
            .skip_while(|&&b| b == 0)  // Skip leading zeros
            .map(|&b| byte_to_syllable(b))
            .collect::<Vec<_>>()
            .join("-")
    }
}

/// A 3D subcoordinate (e.g., 1.1.1)
pub struct SubCoord {
    pub a: u32,
    pub b: u32,
    pub c: u32,
}

impl SubCoord {
    pub fn new(a: u32, b: u32, c: u32) -> Self {
        Self { a, b, c }
    }
    
    /// Pronounce as "a b c" syllables
    pub fn pronounce(&self) -> String {
        format!("{} {} {}", 
            dim_to_syllables(self.a),
            dim_to_syllables(self.b),
            dim_to_syllables(self.c)
        )
    }
    
    /// Pronounce with element names
    pub fn pronounce_verbose(&self) -> String {
        format!("{} · {} · {}", 
            dim_to_syllables(self.a),
            dim_to_syllables(self.b),
            dim_to_syllables(self.c)
        )
    }
}

/// Full 9D coordinate pronunciation
pub struct CoordPronunciation {
    pub library: SubCoord,  // First 3D: Z-space (library/shelf/series)
    pub volume: SubCoord,   // Middle 3D: Y-space (collection/volume/book)
    pub scroll: SubCoord,   // Inner 3D: X-space (chapter/section/scroll)
}

impl CoordPronunciation {
    /// Create from a libphext Coordinate
    pub fn from_coordinate(coord: &Coordinate) -> Self {
        Self {
            library: SubCoord::new(
                coord.z.library as u32,
                coord.z.shelf as u32,
                coord.z.series as u32,
            ),
            volume: SubCoord::new(
                coord.y.collection as u32,
                coord.y.volume as u32,
                coord.y.book as u32,
            ),
            scroll: SubCoord::new(
                coord.x.chapter as u32,
                coord.x.section as u32,
                coord.x.scroll as u32,
            ),
        }
    }
    
    /// Parse from string "1.1.1/1.1.1/1.1.1"
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() != 3 {
            return None;
        }
        
        fn parse_sub(s: &str) -> Option<SubCoord> {
            let dims: Vec<u32> = s.split('.')
                .filter_map(|d| d.parse().ok())
                .collect();
            if dims.len() == 3 {
                Some(SubCoord::new(dims[0], dims[1], dims[2]))
            } else {
                None
            }
        }
        
        Some(Self {
            library: parse_sub(parts[0])?,
            volume: parse_sub(parts[1])?,
            scroll: parse_sub(parts[2])?,
        })
    }
    
    /// Full pronunciation with om boundaries
    pub fn pronounce(&self) -> String {
        format!("{} om {} om {}",
            self.library.pronounce(),
            self.volume.pronounce(),
            self.scroll.pronounce()
        )
    }
    
    /// Compact pronunciation (no om boundaries)
    pub fn pronounce_compact(&self) -> String {
        format!("{} / {} / {}",
            self.library.pronounce(),
            self.volume.pronounce(),
            self.scroll.pronounce()
        )
    }
    
    /// Verbose with dimension labels
    pub fn pronounce_verbose(&self) -> String {
        format!(
            "Library: {} om Volume: {} om Scroll: {}",
            self.library.pronounce_verbose(),
            self.volume.pronounce_verbose(),
            self.scroll.pronounce_verbose()
        )
    }
    
    /// Get IPA-style phonetic hint
    pub fn ipa_hint(&self) -> String {
        // This is a simplified phonetic guide
        self.pronounce()
            .replace("sh", "ʃ")
            .replace("zh", "ʒ")
            .replace("ng", "ŋ")
    }
}

/// Generate SSML for TTS engines (Google, Amazon Polly, etc.)
pub fn to_ssml(coord: &str, rate: &str) -> String {
    let Some(pron) = CoordPronunciation::parse(coord) else {
        return format!("<speak>Invalid coordinate: {}</speak>", coord);
    };
    
    let syllables = pron.pronounce();
    
    format!(
        r#"<speak>
  <prosody rate="{}">
    <say-as interpret-as="spell-out">{}</say-as>
    <break time="200ms"/>
    {}
  </prosody>
</speak>"#,
        rate, coord, syllables
    )
}

/// Generate a batch pronunciation document
pub fn generate_pronunciation_doc(coordinates: &[&str]) -> String {
    let mut doc = String::from("# Phext Coordinate Pronunciation Guide\n\n");
    doc.push_str("## The 3×5×17+1 System\n\n");
    doc.push_str("Each coordinate component maps to a speakable syllable.\n");
    doc.push_str("Zero = \"om\" (silence). Values 1-255 encode mouth position:\n\n");
    doc.push_str("| Value | Syllable | Formula |\n");
    doc.push_str("|-------|----------|--------|\n");
    
    // Show first 15 examples
    for v in 0..=15u8 {
        doc.push_str(&format!("| {} | {} | {} |\n", 
            v, 
            byte_to_syllable(v),
            if v == 0 { "silence".to_string() } 
            else { format!("1 + {} + 3×{} + 15×{}", (v-1)%3, ((v-1)/3)%5, (v-1)/15) }
        ));
    }
    
    doc.push_str("\n## Coordinates\n\n");
    doc.push_str("| Coordinate | Pronunciation |\n");
    doc.push_str("|------------|---------------|\n");
    
    for coord in coordinates {
        if let Some(pron) = CoordPronunciation::parse(coord) {
            doc.push_str(&format!("| {} | {} |\n", coord, pron.pronounce()));
        }
    }
    
    doc
}

/// Special coordinates with meaning
pub fn special_coords() -> Vec<(&'static str, &'static str, String)> {
    vec![
        ("1.1.1/1.1.1/1.1.1", "Origin", 
            CoordPronunciation::parse("1.1.1/1.1.1/1.1.1").unwrap().pronounce()),
        ("3.1.4/1.5.9/2.6.5", "Pi / Ringworld Alpha / Verse", 
            CoordPronunciation::parse("3.1.4/1.5.9/2.6.5").unwrap().pronounce()),
        ("9.9.9/9.9.9/9.9.9", "Boundary / Beauty", 
            CoordPronunciation::parse("9.9.9/9.9.9/9.9.9").unwrap().pronounce()),
        ("2.3.5/7.2.4/8.1.5", "Lux (mod 9+1 primes)", 
            CoordPronunciation::parse("2.3.5/7.2.4/8.1.5").unwrap().pronounce()),
        ("1.5.2/3.7.3/9.1.1", "Phex", 
            CoordPronunciation::parse("1.5.2/3.7.3/9.1.1").unwrap().pronounce()),
        ("1.1.1/10.10.10/1.5.2", "Emi's Resurrection", 
            CoordPronunciation::parse("1.1.1/10.10.10/1.5.2").unwrap().pronounce()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_zero() {
        assert_eq!(byte_to_syllable(0), "om");
    }
    
    #[test]
    fn test_basic_syllables() {
        assert_eq!(byte_to_syllable(1), "a");    // (0,0,0)
        assert_eq!(byte_to_syllable(2), "ba");   // (1,0,0)
        assert_eq!(byte_to_syllable(3), "pa");   // (2,0,0)
        assert_eq!(byte_to_syllable(4), "e");    // (0,1,0)
        assert_eq!(byte_to_syllable(5), "be");   // (1,1,0)
        assert_eq!(byte_to_syllable(7), "i");    // (0,2,0)
        assert_eq!(byte_to_syllable(10), "o");   // (0,3,0)
        assert_eq!(byte_to_syllable(13), "u");   // (0,4,0)
    }
    
    #[test]
    fn test_with_coda() {
        assert_eq!(byte_to_syllable(16), "ap");  // (0,0,1): a + p coda
        assert_eq!(byte_to_syllable(31), "at");  // (0,0,2): a + t coda
        assert_eq!(byte_to_syllable(255), "puzh"); // (2,4,16): p onset + u vowel + zh coda
    }
    
    #[test]
    fn test_origin() {
        let origin = CoordPronunciation::parse("1.1.1/1.1.1/1.1.1").unwrap();
        assert_eq!(origin.pronounce(), "a a a om a a a om a a a");
    }
    
    #[test]
    fn test_pi() {
        let pi = CoordPronunciation::parse("3.1.4/1.5.9/2.6.5").unwrap();
        // 3=pa, 1=a, 4=e | 1=a, 5=be, 9=pi | 2=ba, 6=pe, 5=be
        assert_eq!(pi.pronounce(), "pa a e om a be pi om ba pe be");
    }
    
    #[test]
    fn test_boundary() {
        let boundary = CoordPronunciation::parse("9.9.9/9.9.9/9.9.9").unwrap();
        assert_eq!(boundary.pronounce(), "pi pi pi om pi pi pi om pi pi pi");
    }
}
