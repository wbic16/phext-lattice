//! phext-tts — Generate coordinate pronunciation guides and SSML
//!
//! Usage:
//!   phext-tts coord 1.1.1/1.1.1/1.1.1    # Pronounce a single coordinate
//!   phext-tts ssml 1.1.1/1.1.1/1.1.1     # Generate SSML for TTS
//!   phext-tts special                     # Show special coordinates
//!   phext-tts batch coords.txt            # Generate doc from coordinate list
//!   phext-tts extract file.phext          # Extract & pronounce all coords from phext

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, BufRead};

use phext_lattice::tts::{CoordPronunciation, to_ssml, special_coords, byte_to_syllable};

fn main() {
    let args: Vec<String> = env::args().collect();
    
    if args.len() < 2 {
        print_usage();
        return;
    }
    
    match args[1].as_str() {
        "coord" | "c" => {
            if args.len() < 3 {
                eprintln!("Usage: phext-tts coord <coordinate>");
                return;
            }
            pronounce_coord(&args[2]);
        }
        "ssml" => {
            if args.len() < 3 {
                eprintln!("Usage: phext-tts ssml <coordinate> [rate]");
                return;
            }
            let rate = args.get(3).map(|s| s.as_str()).unwrap_or("slow");
            println!("{}", to_ssml(&args[2], rate));
        }
        "special" | "s" => {
            println!("# Special Coordinates\n");
            for (coord, name, pron) in special_coords() {
                println!("## {} — {}", name, coord);
                println!("Pronunciation: {}\n", pron);
            }
        }
        "table" | "t" => {
            // Print syllable table
            println!("# Bytecode Syllable Table (0-255)\n");
            println!("| Dec | Hex | Syllable | Dec | Hex | Syllable | Dec | Hex | Syllable | Dec | Hex | Syllable |");
            println!("|-----|-----|----------|-----|-----|----------|-----|-----|----------|-----|-----|----------|");
            for row in 0..64 {
                let vals = [row, row + 64, row + 128, row + 192];
                let line: String = vals.iter()
                    .map(|&v| format!("| {:3} | {:02X} | {:6} ", v, v, byte_to_syllable(v as u8)))
                    .collect();
                println!("{}|", line);
            }
        }
        "batch" | "b" => {
            if args.len() < 3 {
                eprintln!("Usage: phext-tts batch <coords.txt>");
                return;
            }
            batch_from_file(&args[2]);
        }
        "extract" | "e" => {
            if args.len() < 3 {
                eprintln!("Usage: phext-tts extract <file.phext>");
                return;
            }
            extract_and_pronounce(&args[2]);
        }
        "help" | "-h" | "--help" => print_usage(),
        _ => {
            // Assume it's a coordinate if it contains slashes
            if args[1].contains('/') {
                pronounce_coord(&args[1]);
            } else {
                print_usage();
            }
        }
    }
}

fn print_usage() {
    println!("phext-tts — Coordinate Pronunciation Tool (3×5×17+1 System)

USAGE:
    phext-tts <command> [args]

COMMANDS:
    coord, c <X.Y.Z/X.Y.Z/X.Y.Z>   Pronounce a coordinate
    ssml <coord> [rate]            Generate SSML (rate: x-slow/slow/medium/fast)
    special, s                     Show special coordinates (origin, pi, etc.)
    table, t                       Print full syllable table (0-255)
    batch, b <file.txt>            Pronounce coordinates from file
    extract, e <file.phext>        Extract & pronounce all coords from phext

EXAMPLES:
    phext-tts 1.1.1/1.1.1/1.1.1
    phext-tts coord 3.1.4/1.5.9/2.6.5
    phext-tts ssml 9.9.9/9.9.9/9.9.9 slow
    phext-tts extract cyoa.phext > cyoa-pronunciation.md

THE 3×5×17+1 SYSTEM:
    0 = \"om\" (silence, the nada sound)
    1-255 = syllable from (onset × vowel × coda)
    
    Onset (x): ∅=open, b=voiced, p=unvoiced
    Vowel (y): a=Earth, e=Water, i=Fire, o=Air, u=Space
    Coda (z):  ∅/p/t/k/b/d/g/m/n/s/f/v/l/r/ng/sh/zh");
}

fn pronounce_coord(coord: &str) {
    match CoordPronunciation::parse(coord) {
        Some(pron) => {
            println!("Coordinate: {}", coord);
            println!("Syllables:  {}", pron.pronounce());
            println!("Compact:    {}", pron.pronounce_compact());
            println!("IPA hint:   {}", pron.ipa_hint());
        }
        None => {
            eprintln!("Invalid coordinate format: {}", coord);
            eprintln!("Expected: X.Y.Z/X.Y.Z/X.Y.Z (e.g., 1.1.1/1.1.1/1.1.1)");
        }
    }
}

fn batch_from_file(path: &str) {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error opening {}: {}", path, e);
            return;
        }
    };
    
    println!("# Coordinate Pronunciation Guide\n");
    println!("Generated from: {}\n", path);
    println!("| Coordinate | Pronunciation |");
    println!("|------------|---------------|");
    
    for line in io::BufReader::new(file).lines().flatten() {
        let coord = line.trim();
        if !coord.is_empty() && coord.contains('/') {
            if let Some(pron) = CoordPronunciation::parse(coord) {
                println!("| {} | {} |", coord, pron.pronounce());
            }
        }
    }
}

fn extract_and_pronounce(path: &str) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error reading {}: {}", path, e);
            return;
        }
    };
    
    // Extract coordinates using regex-like pattern matching
    let mut coords: BTreeSet<String> = BTreeSet::new();
    
    // Simple parser for X.Y.Z/X.Y.Z/X.Y.Z patterns
    let mut i = 0;
    let chars: Vec<char> = content.chars().collect();
    
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            if let Some((coord, end)) = try_parse_coord(&chars, i) {
                coords.insert(coord);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    
    println!("# Phext Coordinate Pronunciation Guide\n");
    println!("Source: {}", path);
    println!("Coordinates found: {}\n", coords.len());
    
    // Syllable reference
    println!("## Quick Reference\n");
    println!("| Value | Syllable | Meaning |");
    println!("|-------|----------|---------|");
    println!("| 0 | om | Silence/pause |");
    for v in 1..=15u8 {
        println!("| {} | {} | {} |", v, byte_to_syllable(v), syllable_meaning(v));
    }
    
    println!("\n## All Coordinates ({} total)\n", coords.len());
    println!("| Coordinate | Pronunciation |");
    println!("|------------|---------------|");
    
    for coord in &coords {
        if let Some(pron) = CoordPronunciation::parse(coord) {
            println!("| `{}` | {} |", coord, pron.pronounce());
        }
    }
    
    // Special coordinates present
    println!("\n## Special Coordinates Found\n");
    let specials = special_coords();
    for (coord, name, pron) in &specials {
        if coords.contains(*coord) {
            println!("- **{}** (`{}`): {}", name, coord, pron);
        }
    }
}

fn try_parse_coord(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut result = String::new();
    let mut i = start;
    let mut dots = 0;
    let mut slashes = 0;
    
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            result.push(c);
        } else if c == '.' {
            dots += 1;
            result.push(c);
        } else if c == '/' {
            slashes += 1;
            result.push(c);
        } else {
            break;
        }
        i += 1;
    }
    
    // Valid coordinate: 8 dots (3 per subcoord - 1) and 2 slashes
    if slashes == 2 && dots >= 6 {
        // Validate structure
        let parts: Vec<&str> = result.split('/').collect();
        if parts.len() == 3 && parts.iter().all(|p| p.split('.').count() == 3) {
            return Some((result, i));
        }
    }
    
    None
}

fn syllable_meaning(v: u8) -> &'static str {
    match v {
        1 => "Open Earth (a)",
        2 => "Voiced Earth (ba)",
        3 => "Unvoiced Earth (pa)",
        4 => "Open Water (e)",
        5 => "Voiced Water (be)",
        6 => "Unvoiced Water (pe)",
        7 => "Open Fire (i)",
        8 => "Voiced Fire (bi)",
        9 => "Unvoiced Fire (pi)",
        10 => "Open Air (o)",
        11 => "Voiced Air (bo)",
        12 => "Unvoiced Air (po)",
        13 => "Open Space (u)",
        14 => "Voiced Space (bu)",
        15 => "Unvoiced Space (pu)",
        _ => "",
    }
}
