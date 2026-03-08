# Phext TTS — The 3×5×17+1 Pronunciation System

Voice navigation for phext coordinates. Each byte value maps to a speakable syllable.

## The System

```
0 = "om" (silence, the nada sound)
1-255 = syllable from (onset × vowel × coda)

Onset (x): ∅=open, b=voiced, p=unvoiced        [3 values]
Vowel (y): a=Earth, e=Water, i=Fire, o=Air, u=Space  [5 values]
Coda (z):  ∅/p/t/k/b/d/g/m/n/s/f/v/l/r/ng/sh/zh   [17 values]

Formula: value = 1 + x + 3y + 15z
```

## Files

- **phext-tts.js** — Browser module with Web Speech API integration
- **tts-demo.html** — Interactive demo page

## Usage (Browser)

```html
<script src="phext-tts.js"></script>
<script>
  // Initialize (call once)
  PhextTTS.initTTS();
  
  // Get pronunciation
  PhextTTS.pronounce('1.1.1/1.1.1/1.1.1');
  // → "a a a om a a a om a a a"
  
  // Speak aloud
  await PhextTTS.speak('3.1.4/1.5.9/2.6.5');
  
  // Auto-enhance coordinate elements
  PhextTTS.enhanceCoordinates('[data-coordinate]');
</script>
```

## Usage (Rust CLI)

```bash
# Build
cargo build --bin phext-tts

# Pronounce a coordinate
phext-tts 1.1.1/1.1.1/1.1.1

# Generate SSML for external TTS
phext-tts ssml 3.1.4/1.5.9/2.6.5 slow

# Extract all coordinates from a phext
phext-tts extract file.phext > pronunciation.md

# Show special coordinates
phext-tts special
```

## Special Coordinates

| Coordinate | Name | Pronunciation |
|------------|------|---------------|
| 1.1.1/1.1.1/1.1.1 | Origin | a a a om a a a om a a a |
| 3.1.4/1.5.9/2.6.5 | Pi / Verse | pa a e om a be pi om ba pe be |
| 9.9.9/9.9.9/9.9.9 | Boundary | pi pi pi om pi pi pi om pi pi pi |
| 2.3.5/7.2.4/8.1.5 | Lux | ba pa be om i ba e om bi a be |
| 1.5.2/3.7.3/9.1.1 | Phex | a be ba om pa i pa om pi a a |

## Why 3×5×17+1?

> "The Federation suggests having a special number for 0, the nada sound, the pause between all words... The relatively prime structure also breaks up aliasing fencepost problems when thinking in powers of 2."
> 
> — Eigenhector Federation, "Dimensional Debugging"

The factors 3, 5, and 17 share no common divisors. This prevents the aliasing problems that plague power-of-2 encodings.

## Design Principle

3D space + 1D time:
- **3D Space**: Each syllable locates a point in mouth-space (onset × vowel × coda)
- **1D Time**: The sequence of syllables traces a path through this space

When speaking bytecode, you are literally navigating coordinate space with your mouth.

## License

MIT
