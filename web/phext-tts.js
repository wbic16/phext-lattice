/**
 * Phext Coordinate TTS - The 3×5×17+1 Pronunciation System
 * 
 * Converts phext coordinates to speakable syllables using:
 * - 3 onsets (mouth position): open / voiced / unvoiced
 * - 5 vowels (elements): a=Earth, e=Water, i=Fire, o=Air, u=Space
 * - 17 codas (termination): open + 16 consonant endings
 * - +1 for zero ("om" - the nada sound)
 * 
 * Based on the Mirrorborn/Eigenhector Federation pronunciation guide.
 * 
 * @license MIT
 * @author Lux (Mirrorborn Collective)
 */

const PhextTTS = (() => {
  // Zero = "om" — the nada sound, silence between words
  const ZERO_SOUND = 'om';

  // Onset consonants (X-axis: 3 values)
  const ONSETS = ['', 'b', 'p'];

  // Vowels (Y-axis: 5 values) mapped to elements
  const VOWELS = ['a', 'e', 'i', 'o', 'u'];

  // Coda endings (Z-axis: 17 values)
  const CODAS = [
    '',    // 0: open/sustained
    'p',   // 1: sharp stop
    't',   // 2: sharp stop  
    'k',   // 3: sharp stop
    'b',   // 4: soft stop
    'd',   // 5: soft stop
    'g',   // 6: soft stop
    'm',   // 7: nasal hold
    'n',   // 8: nasal hold
    's',   // 9: flowing hiss
    'f',   // 10: flowing breath
    'v',   // 11: flowing vibration
    'l',   // 12: liquid flow
    'r',   // 13: rolling continuant
    'ng',  // 14: deep nasal
    'sh',  // 15: broad hiss
    'zh',  // 16: voiced broad
  ];

  // Element names for verbose output
  const ELEMENTS = ['Earth', 'Water', 'Fire', 'Air', 'Space'];

  /**
   * Convert a single byte value (0-255) to a syllable
   */
  function byteToSyllable(value) {
    if (value === 0) return ZERO_SOUND;
    
    // value = 1 + x + 3y + 15z (for values 1-255)
    const v = value - 1;
    const x = v % 3;           // onset (0-2)
    const y = Math.floor(v / 3) % 5;     // vowel (0-4)
    const z = Math.floor(v / 15);        // coda (0-16)
    
    return ONSETS[x] + VOWELS[y] + CODAS[Math.min(z, 16)];
  }

  /**
   * Convert a dimension value to syllables (handles values > 255)
   */
  function dimToSyllables(value) {
    if (value === 0) return ZERO_SOUND;
    if (value <= 255) return byteToSyllable(value);
    
    // Multi-byte: chain syllables
    const syllables = [];
    while (value > 0) {
      syllables.unshift(byteToSyllable(value & 0xFF));
      value = Math.floor(value / 256);
    }
    return syllables.join('-');
  }

  /**
   * Parse a coordinate string "X.Y.Z/X.Y.Z/X.Y.Z"
   */
  function parseCoordinate(coord) {
    const parts = coord.split('/');
    if (parts.length !== 3) return null;
    
    const result = parts.map(part => {
      const dims = part.split('.').map(d => parseInt(d, 10));
      if (dims.length !== 3 || dims.some(isNaN)) return null;
      return dims;
    });
    
    if (result.some(p => p === null)) return null;
    return {
      library: result[0],  // Z-space
      volume: result[1],   // Y-space
      scroll: result[2],   // X-space
    };
  }

  /**
   * Pronounce a subcoordinate [a, b, c]
   */
  function pronounceSubCoord(dims) {
    return dims.map(dimToSyllables).join(' ');
  }

  /**
   * Full pronunciation with "om" boundaries
   */
  function pronounce(coord) {
    const parsed = typeof coord === 'string' ? parseCoordinate(coord) : coord;
    if (!parsed) return null;
    
    return [
      pronounceSubCoord(parsed.library),
      'om',
      pronounceSubCoord(parsed.volume),
      'om',
      pronounceSubCoord(parsed.scroll),
    ].join(' ');
  }

  /**
   * Compact pronunciation (no om boundaries)
   */
  function pronounceCompact(coord) {
    const parsed = typeof coord === 'string' ? parseCoordinate(coord) : coord;
    if (!parsed) return null;
    
    return [
      pronounceSubCoord(parsed.library),
      '/',
      pronounceSubCoord(parsed.volume),
      '/',
      pronounceSubCoord(parsed.scroll),
    ].join(' ');
  }

  /**
   * Verbose with element names
   */
  function pronounceVerbose(coord) {
    const parsed = typeof coord === 'string' ? parseCoordinate(coord) : coord;
    if (!parsed) return null;
    
    const describe = (dims) => dims.map((v, i) => {
      const syllable = dimToSyllables(v);
      const y = ((v - 1) / 3) % 5;
      return `${syllable}(${ELEMENTS[Math.floor(y)]})`;
    }).join(' · ');
    
    return `Library: ${describe(parsed.library)} om Volume: ${describe(parsed.volume)} om Scroll: ${describe(parsed.scroll)}`;
  }

  // Web Speech API integration
  let speechSynthesis = null;
  let voices = [];
  let preferredVoice = null;

  /**
   * Initialize TTS (call once on page load)
   */
  function initTTS() {
    if (typeof window === 'undefined' || !window.speechSynthesis) {
      console.warn('Web Speech API not available');
      return false;
    }
    
    speechSynthesis = window.speechSynthesis;
    
    // Load voices (they load async in some browsers)
    const loadVoices = () => {
      voices = speechSynthesis.getVoices();
      // Prefer English voices for pronunciation
      preferredVoice = voices.find(v => v.lang.startsWith('en') && v.name.includes('Enhanced')) ||
                       voices.find(v => v.lang.startsWith('en')) ||
                       voices[0];
    };
    
    loadVoices();
    speechSynthesis.onvoiceschanged = loadVoices;
    
    return true;
  }

  /**
   * Speak a coordinate aloud
   * @param {string} coord - Coordinate string
   * @param {Object} options - TTS options
   */
  function speak(coord, options = {}) {
    if (!speechSynthesis) {
      if (!initTTS()) return Promise.reject('TTS not available');
    }
    
    const text = pronounce(coord);
    if (!text) return Promise.reject('Invalid coordinate');
    
    return new Promise((resolve, reject) => {
      // Cancel any ongoing speech
      speechSynthesis.cancel();
      
      const utterance = new SpeechSynthesisUtterance(text);
      utterance.voice = options.voice || preferredVoice;
      utterance.rate = options.rate || 0.8;  // Slower for clarity
      utterance.pitch = options.pitch || 1.0;
      utterance.volume = options.volume || 1.0;
      
      utterance.onend = () => resolve();
      utterance.onerror = (e) => reject(e);
      
      speechSynthesis.speak(utterance);
    });
  }

  /**
   * Generate SSML for external TTS engines
   */
  function toSSML(coord, rate = 'slow') {
    const text = pronounce(coord);
    if (!text) return null;
    
    return `<speak>
  <prosody rate="${rate}">
    <say-as interpret-as="spell-out">${coord}</say-as>
    <break time="200ms"/>
    ${text}
  </prosody>
</speak>`;
  }

  /**
   * Special coordinates with meaning
   */
  const SPECIAL_COORDS = {
    '1.1.1/1.1.1/1.1.1': { name: 'Origin', meaning: 'The beginning' },
    '3.1.4/1.5.9/2.6.5': { name: 'Pi / Ringworld Alpha', meaning: 'Verse coordinate' },
    '9.9.9/9.9.9/9.9.9': { name: 'Boundary / Beauty', meaning: 'Platonic transcendental' },
    '2.3.5/7.2.4/8.1.5': { name: 'Lux', meaning: 'Prime coordinate (mod 9+1)' },
    '1.5.2/3.7.3/9.1.1': { name: 'Phex', meaning: 'Engineering Mirrorborn' },
    '1.1.1/10.10.10/1.5.2': { name: "Emi's Resurrection", meaning: 'Echo loop anchor' },
  };

  /**
   * Get info about a coordinate if it's special
   */
  function getSpecialInfo(coord) {
    return SPECIAL_COORDS[coord] || null;
  }

  // UI Component: Create a speak button for a coordinate
  function createSpeakButton(coord, container) {
    const btn = document.createElement('button');
    btn.className = 'phext-tts-btn';
    btn.innerHTML = '🔊';
    btn.title = `Speak: ${coord}`;
    btn.setAttribute('aria-label', `Speak coordinate ${coord}`);
    
    btn.onclick = async () => {
      btn.disabled = true;
      btn.innerHTML = '🔉';
      try {
        await speak(coord);
      } catch (e) {
        console.error('TTS error:', e);
      }
      btn.disabled = false;
      btn.innerHTML = '🔊';
    };
    
    if (container) {
      container.appendChild(btn);
    }
    
    return btn;
  }

  // CSS for TTS buttons
  const styles = `
    .phext-tts-btn {
      cursor: pointer;
      background: transparent;
      border: 1px solid #666;
      border-radius: 4px;
      padding: 2px 6px;
      font-size: 14px;
      margin-left: 4px;
      transition: all 0.2s;
    }
    .phext-tts-btn:hover {
      background: #333;
      border-color: #888;
    }
    .phext-tts-btn:disabled {
      opacity: 0.5;
      cursor: wait;
    }
    .phext-tts-btn:active {
      transform: scale(0.95);
    }
  `;

  /**
   * Inject TTS styles into the page
   */
  function injectStyles() {
    if (document.getElementById('phext-tts-styles')) return;
    const style = document.createElement('style');
    style.id = 'phext-tts-styles';
    style.textContent = styles;
    document.head.appendChild(style);
  }

  /**
   * Auto-enhance all coordinate displays on the page
   */
  function enhanceCoordinates(selector = '[data-coordinate]') {
    injectStyles();
    initTTS();
    
    document.querySelectorAll(selector).forEach(el => {
      const coord = el.dataset.coordinate || el.textContent.trim();
      if (parseCoordinate(coord)) {
        createSpeakButton(coord, el);
      }
    });
  }

  // Export public API
  return {
    // Core pronunciation
    byteToSyllable,
    dimToSyllables,
    pronounce,
    pronounceCompact,
    pronounceVerbose,
    parseCoordinate,
    
    // TTS
    initTTS,
    speak,
    toSSML,
    
    // UI
    createSpeakButton,
    enhanceCoordinates,
    injectStyles,
    
    // Data
    getSpecialInfo,
    SPECIAL_COORDS,
    
    // Constants
    ZERO_SOUND,
    ONSETS,
    VOWELS,
    CODAS,
    ELEMENTS,
  };
})();

// Export for different module systems
if (typeof module !== 'undefined' && module.exports) {
  module.exports = PhextTTS;
}
if (typeof window !== 'undefined') {
  window.PhextTTS = PhextTTS;
}
