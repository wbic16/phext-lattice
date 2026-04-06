/// 6D UML — SDLC semantic layer over phext coordinates.
///
/// Maps the 6 inner phext dimensions to software engineering concepts:
///   Collection (8D) = SDLC Phase
///   Volume     (7D) = Sprint/Release
///   Book       (6D) = Module
///   Chapter    (5D) = Component
///   Section    (4D) = File
///   Scroll     (3D) = Unit
///
/// The outer 3 dimensions (Series, Shelf, Library) are user-managed.
///
/// An artifact's 5D address (volume.book.chapter/section.scroll) is stable
/// across all SDLC phases. Advancing collection traces an artifact from
/// its requirement through design, implementation, testing, and docs.

use libphext::phext::Coordinate;
use crate::index::LatticeIndex;

/// The 9 SDLC phases mapped to collection values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum Phase {
    Meta         = 1,
    Requirements = 2,
    Design       = 3,
    Source        = 4,
    Tests         = 5,
    Regressions   = 6,
    Docs          = 7,
    Whitepapers   = 8,
    Training      = 9,
}

impl Phase {
    pub fn from_collection(c: usize) -> Option<Phase> {
        match c {
            1 => Some(Phase::Meta),
            2 => Some(Phase::Requirements),
            3 => Some(Phase::Design),
            4 => Some(Phase::Source),
            5 => Some(Phase::Tests),
            6 => Some(Phase::Regressions),
            7 => Some(Phase::Docs),
            8 => Some(Phase::Whitepapers),
            9 => Some(Phase::Training),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Phase::Meta         => "Meta",
            Phase::Requirements => "Requirements",
            Phase::Design       => "Design",
            Phase::Source        => "Source",
            Phase::Tests         => "Tests",
            Phase::Regressions   => "Regressions",
            Phase::Docs          => "Docs",
            Phase::Whitepapers   => "Whitepapers",
            Phase::Training      => "Training",
        }
    }

    pub fn short(&self) -> &'static str {
        match self {
            Phase::Meta         => "META",
            Phase::Requirements => "REQ",
            Phase::Design       => "DES",
            Phase::Source        => "SRC",
            Phase::Tests         => "TST",
            Phase::Regressions   => "REG",
            Phase::Docs          => "DOC",
            Phase::Whitepapers   => "WHP",
            Phase::Training      => "TRN",
        }
    }

    /// CSS color for this phase.
    pub fn color(&self) -> &'static str {
        match self {
            Phase::Meta         => "#808080",
            Phase::Requirements => "#e8c547",
            Phase::Design       => "#c964c9",
            Phase::Source        => "#6bc96b",
            Phase::Tests         => "#4dc9c9",
            Phase::Regressions   => "#e07040",
            Phase::Docs          => "#7090e0",
            Phase::Whitepapers   => "#b0b0b0",
            Phase::Training      => "#d4a855",
        }
    }

    /// All phases in order.
    pub fn all() -> &'static [Phase] {
        &[
            Phase::Meta, Phase::Requirements, Phase::Design,
            Phase::Source, Phase::Tests, Phase::Regressions,
            Phase::Docs, Phase::Whitepapers, Phase::Training,
        ]
    }
}

/// A 5D artifact address — stable across SDLC phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactAddr {
    pub volume:  usize, // Sprint/Release
    pub book:    usize, // Module
    pub chapter: usize, // Component
    pub section: usize, // File
    pub scroll:  usize, // Unit
}

impl ArtifactAddr {
    /// Extract the 5D artifact address from a full coordinate.
    pub fn from_coordinate(c: &Coordinate) -> Self {
        ArtifactAddr {
            volume:  c.y.volume,
            book:    c.y.book,
            chapter: c.x.chapter,
            section: c.x.section,
            scroll:  c.x.scroll,
        }
    }

    /// Build a full coordinate by combining this address with an SDLC phase
    /// and outer dimensions (defaulting outer to 1.1.1).
    pub fn to_coordinate(&self, phase: Phase) -> Coordinate {
        self.to_coordinate_with_outer(phase, 1, 1, 1)
    }

    /// Build a full coordinate with explicit outer dimensions.
    pub fn to_coordinate_with_outer(
        &self,
        phase: Phase,
        library: usize,
        shelf: usize,
        series: usize,
    ) -> Coordinate {
        Coordinate {
            z: libphext::phext::ZCoordinate { library, shelf, series },
            y: libphext::phext::YCoordinate {
                collection: phase as usize,
                volume: self.volume,
                book: self.book,
            },
            x: libphext::phext::XCoordinate {
                chapter: self.chapter,
                section: self.section,
                scroll: self.scroll,
            },
        }
    }

    /// Display as volume.book.chapter/section.scroll
    pub fn display(&self) -> String {
        format!("{}.{}.{}/{}.{}", self.volume, self.book, self.chapter, self.section, self.scroll)
    }
}

impl std::fmt::Display for ArtifactAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}/{}.{}", self.volume, self.book, self.chapter, self.section, self.scroll)
    }
}

/// Full 6D UML context for a coordinate.
#[derive(Debug, Clone)]
pub struct UmlContext {
    pub phase: Option<Phase>,
    pub phase_name: String,
    pub artifact: ArtifactAddr,
    pub artifact_label: String,
}

impl UmlContext {
    pub fn from_coordinate(c: &Coordinate) -> Self {
        let phase = Phase::from_collection(c.y.collection);
        let artifact = ArtifactAddr::from_coordinate(c);
        UmlContext {
            phase_name: phase.map(|p| p.name().to_string())
                .unwrap_or_else(|| format!("Phase {}", c.y.collection)),
            phase,
            artifact_label: artifact.display(),
            artifact,
        }
    }
}

/// Trace an artifact across all SDLC phases.
/// Returns (phase, has_content) for each populated collection at this 5D address.
pub fn trace_artifact(
    addr: &ArtifactAddr,
    index: &LatticeIndex,
    library: usize,
    shelf: usize,
    series: usize,
) -> Vec<(Phase, bool)> {
    Phase::all().iter().map(|&phase| {
        let coord = addr.to_coordinate_with_outer(phase, library, shelf, series);
        let populated = index.contains(&coord);
        (phase, populated)
    }).collect()
}

/// Find all populated 5D addresses within a given phase.
pub fn artifacts_in_phase(
    phase: Phase,
    index: &LatticeIndex,
    coords: &[Coordinate],
) -> Vec<ArtifactAddr> {
    let mut addrs: Vec<ArtifactAddr> = coords.iter()
        .filter(|c| c.y.collection == phase as usize)
        .map(|c| ArtifactAddr::from_coordinate(c))
        .collect();
    addrs.sort_by(|a, b| {
        (a.volume, a.book, a.chapter, a.section, a.scroll)
            .cmp(&(b.volume, b.book, b.chapter, b.section, b.scroll))
    });
    addrs.dedup();
    let _ = index; // used for filtering; coords already filtered
    addrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;

    #[test]
    fn phase_roundtrip() {
        for phase in Phase::all() {
            assert_eq!(Phase::from_collection(*phase as usize), Some(*phase));
        }
    }

    #[test]
    fn artifact_addr_from_coordinate() {
        let c = to_coordinate("1.1.1/4.2.3/1.2.5");
        let addr = ArtifactAddr::from_coordinate(&c);
        assert_eq!(addr.volume, 2);
        assert_eq!(addr.book, 3);
        assert_eq!(addr.chapter, 1);
        assert_eq!(addr.section, 2);
        assert_eq!(addr.scroll, 5);
    }

    #[test]
    fn artifact_addr_to_coordinate() {
        let addr = ArtifactAddr { volume: 2, book: 3, chapter: 1, section: 2, scroll: 5 };
        let c = addr.to_coordinate(Phase::Source);
        assert_eq!(c.y.collection, 4); // Source = collection 4
        assert_eq!(c.y.volume, 2);
        assert_eq!(c.y.book, 3);
        assert_eq!(c.x.chapter, 1);
        assert_eq!(c.x.section, 2);
        assert_eq!(c.x.scroll, 5);
    }

    #[test]
    fn context_from_coordinate() {
        let c = to_coordinate("1.1.1/3.1.2/1.1.1");
        let ctx = UmlContext::from_coordinate(&c);
        assert_eq!(ctx.phase, Some(Phase::Design));
        assert_eq!(ctx.phase_name, "Design");
        assert_eq!(ctx.artifact_label, "1.2.1/1.1");
    }

    #[test]
    fn artifact_display() {
        let addr = ArtifactAddr { volume: 1, book: 2, chapter: 3, section: 4, scroll: 5 };
        assert_eq!(addr.display(), "1.2.3/4.5");
    }
}
