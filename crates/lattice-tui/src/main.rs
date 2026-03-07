/// phext-nav — Terminal UI for navigating phext lattices.
///
/// The Shell of Nine interaction model in a terminal.
/// Navigate → Orient → Edit.
///
/// Keys (Lattice mode):
///   1-9       Select dimension
///   h/l ←/→  Move backward/forward in active dimension
///   j/k ↓/↑  Previous/next populated coordinate
///   J/K       Jump 10 populated coordinates
///   g         Goto coordinate (opens input)
///   /         Search across lattice
///   Enter     View scroll content at current coordinate
///   Space     Toggle inline preview
///   m         Set mark
///   '         Jump to mark
///   [/]       History back/forward
///   Tab       Cycle through dimension groups (Z/Y/X)
///   ?         Toggle help overlay
///   i         Info panel (lattice overview)
///   q         Quit

use std::io;
use std::env;
use std::time::Instant;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    execute,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap, List, ListItem, Clear},
    Terminal,
};
use lattice_core::{
    MappedLattice, Navigator, Dimension, CoordinateNav,
    search_lattice_auto, SearchHit,
    ScrollStats, DimensionDensity, LatticeOverview,
    Sentron,
};
use libphext::phext::to_coordinate;

// ── Modes ──────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Clone)]
enum Mode {
    Lattice,
    ScrollView,
    GotoInput,
    SearchInput,
    SearchResults,
    Help,
    Info,
}

// ── App State ──────────────────────────────────────────────────────────

struct App {
    lattice: MappedLattice,
    nav: Navigator,
    mode: Mode,
    previous_mode: Mode,
    input_buffer: String,
    search_hits: Vec<SearchHit>,
    search_cursor: usize,
    status_message: String,
    status_time: Instant,
    scroll_offset: u16,
    show_preview: bool,
    overview: LatticeOverview,
    dimension_densities: Vec<DimensionDensity>,
    sentron: Sentron,
    sentron_stale: bool,
    file_path: String,
}

impl App {
    fn new(lattice: MappedLattice, file_path: String) -> App {
        let buf = lattice.to_phext_bytes();
        let overview = LatticeOverview::build(&buf, lattice.index());
        let dimension_densities: Vec<DimensionDensity> = (1..=9u8)
            .map(|i| DimensionDensity::build(Dimension::from_index(i).unwrap(), lattice.index()))
            .collect();

        let mut nav = Navigator::new();
        if lattice.has_scroll(&libphext::phext::default_coordinate()) {
            nav.goto(libphext::phext::default_coordinate());
        } else {
            nav.next_populated(lattice.index());
        }

        let sentron = Sentron::build(&nav.position(), lattice.index());

        App {
            lattice,
            nav,
            mode: Mode::Lattice,
            previous_mode: Mode::Lattice,
            input_buffer: String::new(),
            search_hits: Vec::new(),
            search_cursor: 0,
            status_message: String::new(),
            status_time: Instant::now(),
            scroll_offset: 0,
            show_preview: true,
            overview,
            dimension_densities,
            sentron,
            sentron_stale: false,
            file_path,
        }
    }

    fn refresh_sentron(&mut self) {
        if self.sentron_stale || self.sentron.center != self.nav.position() {
            self.sentron = Sentron::build(&self.nav.position(), self.lattice.index());
            self.sentron_stale = false;
        }
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = msg.into();
        self.status_time = Instant::now();
    }

    /// Get scroll stats at current position.
    fn current_stats(&self) -> Option<ScrollStats> {
        let pos = self.nav.position();
        self.lattice.index().get(&pos).map(|span| {
            if let Some(raw) = self.lattice.raw_buffer() {
                ScrollStats::from_bytes(&raw[span.start..span.end])
            } else {
                let content = self.lattice.read_scroll(&pos).unwrap_or_default();
                ScrollStats::from_bytes(content.as_bytes())
            }
        })
    }

    /// Position as a breadcrumb with visual hierarchy.
    fn breadcrumb(&self) -> Vec<Span<'static>> {
        let pos = self.nav.position();
        let dim = self.nav.active_dimension();
        let dim_idx = dim as u8;

        let z_style = if dim_idx <= 3 {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let y_style = if dim_idx >= 4 && dim_idx <= 6 {
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let x_style = if dim_idx >= 7 {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let sep = Style::default().fg(Color::DarkGray);

        vec![
            Span::styled(format!("{}.{}.{}", pos.z.library, pos.z.shelf, pos.z.series), z_style),
            Span::styled("/", sep),
            Span::styled(format!("{}.{}.{}", pos.y.collection, pos.y.volume, pos.y.book), y_style),
            Span::styled("/", sep),
            Span::styled(format!("{}.{}.{}", pos.x.chapter, pos.x.section, pos.x.scroll), x_style),
        ]
    }
}

// ── Main ───────────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("phext-nav — navigate 9D phext lattices");
        eprintln!();
        eprintln!("Usage: phext-nav <file.phext>");
        eprintln!();
        eprintln!("Keys:");
        eprintln!("  1-9       Select dimension (Scroll..Library)");
        eprintln!("  h/l       Move backward/forward in dimension");
        eprintln!("  j/k       Next/prev populated coordinate");
        eprintln!("  Enter     View scroll content");
        eprintln!("  /         Search all scrolls");
        eprintln!("  g         Goto coordinate");
        eprintln!("  ?         Help     i  Info     q  Quit");
        std::process::exit(1);
    }

    let file_path = args[1].clone();
    let lattice = MappedLattice::open(&file_path).map_err(|e| {
        eprintln!("Failed to open {}: {}", file_path, e);
        e
    })?;

    let mut app = App::new(lattice, file_path);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    loop {
        app.refresh_sentron();
        terminal.draw(|f| ui(f, &app))?;

        if let Event::Key(key) = event::read()? {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break;
            }

            // Auto-clear status after 3 seconds
            if app.status_time.elapsed().as_secs() > 3 {
                app.status_message.clear();
            }

            let _was_lattice = app.mode == Mode::Lattice;

            match app.mode {
                Mode::Lattice => {
                    if matches!(key.code, KeyCode::Char('q')) {
                        break;
                    }
                    handle_lattice_key(&mut app, key.code, key.modifiers);
                }
                Mode::ScrollView => handle_scroll_key(&mut app, key.code),
                Mode::GotoInput => handle_goto_key(&mut app, key.code),
                Mode::SearchInput => handle_search_key(&mut app, key.code),
                Mode::SearchResults => handle_results_key(&mut app, key.code),
                Mode::Help | Mode::Info => {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Char('i')) {
                        app.mode = app.previous_mode.clone();
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// ── Key Handlers ───────────────────────────────────────────────────────

fn handle_lattice_key(app: &mut App, code: KeyCode, _modifiers: KeyModifiers) {
    match code {
        // Dimension selection
        KeyCode::Char('1') => { app.nav.select_dimension(1); }
        KeyCode::Char('2') => { app.nav.select_dimension(2); }
        KeyCode::Char('3') => { app.nav.select_dimension(3); }
        KeyCode::Char('4') => { app.nav.select_dimension(4); }
        KeyCode::Char('5') => { app.nav.select_dimension(5); }
        KeyCode::Char('6') => { app.nav.select_dimension(6); }
        KeyCode::Char('7') => { app.nav.select_dimension(7); }
        KeyCode::Char('8') => { app.nav.select_dimension(8); }
        KeyCode::Char('9') => { app.nav.select_dimension(9); }

        // Dimension group cycling
        KeyCode::Tab => {
            let dim = app.nav.active_dimension() as u8;
            let next = if dim <= 3 { 4 } else if dim <= 6 { 7 } else { 1 };
            app.nav.select_dimension(next);
        }
        KeyCode::BackTab => {
            let dim = app.nav.active_dimension() as u8;
            let prev = if dim >= 7 { 4 } else if dim >= 4 { 1 } else { 7 };
            app.nav.select_dimension(prev);
        }

        // Movement
        KeyCode::Char('l') | KeyCode::Right => {
            if !app.nav.move_forward() {
                app.set_status("▸ boundary");
            }
        }
        KeyCode::Char('h') | KeyCode::Left => {
            if !app.nav.move_backward() {
                app.set_status("◂ boundary");
            }
        }

        // Populated navigation (J/K = jump 10)
        KeyCode::Char('j') | KeyCode::Down => {
            if !app.nav.next_populated(app.lattice.index()) {
                app.set_status("▾ last scroll");
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if !app.nav.prev_populated(app.lattice.index()) {
                app.set_status("▴ first scroll");
            }
        }
        KeyCode::Char('J') => {
            let mut moved = 0;
            for _ in 0..10 {
                if app.nav.next_populated(app.lattice.index()) { moved += 1; } else { break; }
            }
            if moved > 0 {
                app.set_status(format!("↓{}", moved));
            }
        }
        KeyCode::Char('K') => {
            let mut moved = 0;
            for _ in 0..10 {
                if app.nav.prev_populated(app.lattice.index()) { moved += 1; } else { break; }
            }
            if moved > 0 {
                app.set_status(format!("↑{}", moved));
            }
        }

        // View
        KeyCode::Enter => {
            if app.lattice.has_scroll(&app.nav.position()) {
                app.mode = Mode::ScrollView;
                app.scroll_offset = 0;
            } else {
                app.set_status("○ empty coordinate");
            }
        }
        KeyCode::Char(' ') => {
            app.show_preview = !app.show_preview;
        }

        // Goto / Search
        KeyCode::Char('g') => {
            app.mode = Mode::GotoInput;
            app.input_buffer.clear();
        }
        KeyCode::Char('/') => {
            app.mode = Mode::SearchInput;
            app.input_buffer.clear();
        }

        // Marks
        KeyCode::Char('m') => {
            app.nav.set_mark();
            app.set_status("● mark set");
        }
        KeyCode::Char('\'') => {
            if !app.nav.jump_to_mark() {
                app.set_status("no marks");
            }
        }

        // History
        KeyCode::Char('[') => {
            if !app.nav.history_back() {
                app.set_status("◁ no history");
            }
        }
        KeyCode::Char(']') => {
            if !app.nav.history_forward() {
                app.set_status("▷ at latest");
            }
        }

        // Overlays
        KeyCode::Char('?') => {
            app.previous_mode = Mode::Lattice;
            app.mode = Mode::Help;
        }
        KeyCode::Char('i') => {
            app.previous_mode = Mode::Lattice;
            app.mode = Mode::Info;
        }

        // Home: jump to BASE
        KeyCode::Home => {
            app.nav.goto(libphext::phext::default_coordinate());
            app.set_status("⌂ BASE");
        }

        _ => {}
    }
}

fn handle_scroll_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc | KeyCode::Char('q') => { app.mode = Mode::Lattice; }
        KeyCode::Down | KeyCode::Char('j') => { app.scroll_offset = app.scroll_offset.saturating_add(1); }
        KeyCode::Up | KeyCode::Char('k') => { app.scroll_offset = app.scroll_offset.saturating_sub(1); }
        KeyCode::PageDown | KeyCode::Char('d') => { app.scroll_offset = app.scroll_offset.saturating_add(20); }
        KeyCode::PageUp | KeyCode::Char('u') => { app.scroll_offset = app.scroll_offset.saturating_sub(20); }
        KeyCode::Home | KeyCode::Char('g') => { app.scroll_offset = 0; }
        KeyCode::Char('G') => { app.scroll_offset = u16::MAX; }
        // Navigate to next/prev scroll while in view mode
        KeyCode::Char('n') => {
            if app.nav.next_populated(app.lattice.index()) {
                app.scroll_offset = 0;
                if !app.lattice.has_scroll(&app.nav.position()) {
                    app.mode = Mode::Lattice;
                }
            }
        }
        KeyCode::Char('p') => {
            if app.nav.prev_populated(app.lattice.index()) {
                app.scroll_offset = 0;
                if !app.lattice.has_scroll(&app.nav.position()) {
                    app.mode = Mode::Lattice;
                }
            }
        }
        _ => {}
    }
}

fn handle_goto_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => { app.mode = Mode::Lattice; }
        KeyCode::Enter => {
            let coord = to_coordinate(&app.input_buffer);
            app.nav.goto(coord);
            app.mode = Mode::Lattice;
            let has = if app.lattice.has_scroll(&coord) { "●" } else { "○" };
            app.set_status(format!("→ {} {}", coord, has));
        }
        KeyCode::Char(c) => { app.input_buffer.push(c); }
        KeyCode::Backspace => { app.input_buffer.pop(); }
        _ => {}
    }
}

fn handle_search_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => { app.mode = Mode::Lattice; }
        KeyCode::Enter => {
            let pattern = app.input_buffer.clone();
            let t0 = Instant::now();
            let buf;
            let buffer: &[u8] = if let Some(raw) = app.lattice.raw_buffer() {
                raw
            } else {
                buf = app.lattice.to_phext_bytes();
                &buf
            };
            let hits = search_lattice_auto(buffer, app.lattice.index(), &pattern, false, 500);
            let elapsed = t0.elapsed();
            app.set_status(format!("{} hits for \"{}\" ({:.1}ms)", hits.len(), pattern, elapsed.as_secs_f64() * 1000.0));
            if hits.is_empty() {
                app.mode = Mode::Lattice;
            } else {
                app.search_hits = hits;
                app.search_cursor = 0;
                app.mode = Mode::SearchResults;
            }
        }
        KeyCode::Char(c) => { app.input_buffer.push(c); }
        KeyCode::Backspace => { app.input_buffer.pop(); }
        _ => {}
    }
}

fn handle_results_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc | KeyCode::Char('q') => { app.mode = Mode::Lattice; }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.search_cursor + 1 < app.search_hits.len() {
                app.search_cursor += 1;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.search_cursor = app.search_cursor.saturating_sub(1);
        }
        KeyCode::Enter => {
            if let Some(hit) = app.search_hits.get(app.search_cursor) {
                app.nav.goto(hit.coordinate);
                app.mode = Mode::Lattice;
            }
        }
        _ => {}
    }
}

// ── UI Rendering ───────────────────────────────────────────────────────

fn ui(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Coordinate bar
            Constraint::Min(1),    // Main content
            Constraint::Length(1), // Status
        ])
        .split(f.area());

    render_coordinate_bar(f, app, chunks[0]);

    match &app.mode {
        Mode::Lattice => render_lattice(f, app, chunks[1]),
        Mode::ScrollView => render_scroll(f, app, chunks[1]),
        Mode::GotoInput => render_input(f, "goto:", &app.input_buffer, "coordinate (e.g. 3.3.3/5.1.2/1.5.2)", chunks[1]),
        Mode::SearchInput => render_input(f, "search:", &app.input_buffer, "text to find across all scrolls", chunks[1]),
        Mode::SearchResults => render_search_results(f, app, chunks[1]),
        Mode::Help => {
            render_lattice(f, app, chunks[1]);
            render_help_overlay(f, f.area());
        }
        Mode::Info => {
            render_lattice(f, app, chunks[1]);
            render_info_overlay(f, app, f.area());
        }
    }

    render_status_bar(f, app, chunks[2]);
}

fn render_coordinate_bar(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let dim = app.nav.active_dimension();
    let has_content = app.lattice.has_scroll(&app.nav.position());
    let scroll_count = app.overview.total_scrolls;

    let mut spans = vec![
        Span::styled(" ", Style::default()),
    ];
    spans.extend(app.breadcrumb());
    spans.push(Span::styled(
        format!("  ◆ {} ", dim.name()),
        Style::default().fg(Color::Yellow),
    ));

    // Scroll indicator
    if let Some(stats) = app.current_stats() {
        spans.push(Span::styled(
            format!(" {} {} lines {} words",
                if has_content { "●" } else { "○" },
                stats.line_count,
                stats.word_count),
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::styled(" ○ empty", Style::default().fg(Color::DarkGray)));
    }

    spans.push(Span::styled(
        format!("  [{} scrolls]", scroll_count),
        Style::default().fg(Color::DarkGray),
    ));

    let bar = Paragraph::new(Line::from(spans))
        .block(Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" 💎 phext-nav ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
            .border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(bar, area);
}

fn render_lattice(f: &mut ratatui::Frame, app: &App, area: Rect) {
    // Split: left = dimension map + neighbors, right = content preview
    let cols = if app.show_preview && area.width > 60 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(34),        // Dimension map
                Constraint::Percentage(55),  // Content preview
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(area)
    };

    render_dimension_map(f, app, cols[0]);

    if app.show_preview && cols.len() > 1 {
        render_preview(f, app, cols[1]);
    }
}

fn render_dimension_map(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let pos = app.nav.position();
    let dim = app.nav.active_dimension();
    let sentron = &app.sentron;
    let blocks = ['·', '░', '▒', '▓', '█'];
    let mut lines: Vec<Line> = Vec::new();

    // ── Sentron Header ──
    lines.push(Line::from(vec![
        Span::styled(format!(" ◉ sentron [{}/40]", sentron.size()),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  reach: {}↔{}", sentron.structural_reach, sentron.sequential_reach),
            Style::default().fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(""));

    // ── Spatial Axes (Library, Shelf, Series, Collection) ──
    lines.push(Line::from(Span::styled(
        " ╭─ spatial ──────────────────╮",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
    )));

    for (_i, axon) in sentron.structural_axons().iter().enumerate() {
        let d = axon.dimension;
        let dim_idx = d as u8;
        let val = pos.dimension_value(d);
        let w = axon.weight(sentron.max_axon_total);
        let block = blocks[(w * 4.0).min(4.0) as usize];

        let is_active = d == dim;
        let (marker, style) = if is_active {
            ("▸", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        } else {
            (" ", Style::default().fg(Color::Gray))
        };

        lines.push(Line::from(vec![
            Span::styled(format!(" │{}{} ", marker, dim_idx), style),
            Span::styled(format!("{:<10}", d.name()), style),
            Span::styled(format!("{:>4}", val),
                Style::default().fg(Color::White).add_modifier(if is_active { Modifier::BOLD } else { Modifier::empty() })),
            Span::styled(format!(" −{:>3}", axon.backward_count), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", block), Style::default().fg(Color::Cyan)),
            Span::styled(format!("+{:<3}", axon.forward_count), Style::default().fg(Color::DarkGray)),
            Span::styled(" │", Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)),
        ]));
    }

    lines.push(Line::from(Span::styled(
        " ╰────────────────────────────╯",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
    )));
    lines.push(Line::from(""));

    // ── Temporal Axes (Volume, Book, Chapter, Section) ──
    lines.push(Line::from(Span::styled(
        " ╭─ temporal ─────────────────╮",
        Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM),
    )));

    for (_i, axon) in sentron.sequential_axons().iter().enumerate() {
        let d = axon.dimension;
        let dim_idx = d as u8;
        let val = pos.dimension_value(d);
        let w = axon.weight(sentron.max_axon_total);
        let block = blocks[(w * 4.0).min(4.0) as usize];

        let is_active = d == dim;
        let (marker, style) = if is_active {
            ("▸", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
        } else {
            (" ", Style::default().fg(Color::Gray))
        };

        lines.push(Line::from(vec![
            Span::styled(format!(" │{}{} ", marker, dim_idx), style),
            Span::styled(format!("{:<10}", d.name()), style),
            Span::styled(format!("{:>4}", val),
                Style::default().fg(Color::White).add_modifier(if is_active { Modifier::BOLD } else { Modifier::empty() })),
            Span::styled(format!(" −{:>3}", axon.backward_count), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", block), Style::default().fg(Color::Magenta)),
            Span::styled(format!("+{:<3}", axon.forward_count), Style::default().fg(Color::DarkGray)),
            Span::styled(" │", Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM)),
        ]));
    }

    lines.push(Line::from(Span::styled(
        " ╰────────────────────────────╯",
        Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM),
    )));
    lines.push(Line::from(""));

    // ── Scroll (the neuron itself, dimension 9) ──
    let scroll_density = &app.dimension_densities[8]; // dim 9 = index 8
    let spark = scroll_density.sparkline(10);
    let scroll_marker = if dim == Dimension::Scroll { "▸" } else { " " };
    let scroll_style = if dim == Dimension::Scroll {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    lines.push(Line::from(vec![
        Span::styled(format!(" {}9 ", scroll_marker), scroll_style),
        Span::styled(format!("{:<10}", "Scroll"), scroll_style),
        Span::styled(format!("{:>4}", pos.dimension_value(Dimension::Scroll)),
            Style::default().fg(Color::White).add_modifier(if dim == Dimension::Scroll { Modifier::BOLD } else { Modifier::empty() })),
        Span::styled(format!("  {}", spark), Style::default().fg(Color::Green)),
        Span::styled(" ← neuron", Style::default().fg(Color::DarkGray)),
    ]));

    // ── Neighbors ──
    lines.push(Line::from(""));
    let summary = app.nav.summarize(app.lattice.index());

    if let Some(prev) = summary.prev {
        let preview = app.lattice.read_scroll(&prev)
            .map(|s| s.chars().take(28).collect::<String>())
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(" ▴ ", Style::default().fg(Color::Blue)),
            Span::styled(format!("{}", prev), Style::default().fg(Color::Blue)),
        ]));
        if !preview.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default()),
                Span::styled(preview.replace('\n', " "), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    if let Some(next) = summary.next {
        let preview = app.lattice.read_scroll(&next)
            .map(|s| s.chars().take(28).collect::<String>())
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(" ▾ ", Style::default().fg(Color::Blue)),
            Span::styled(format!("{}", next), Style::default().fg(Color::Blue)),
        ]));
        if !preview.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default()),
                Span::styled(preview.replace('\n', " "), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    let title = format!(" ◉ Sentron [2×4 × 5×8] ");
    let widget = Paragraph::new(Text::from(lines))
        .block(Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(title, Style::default().fg(Color::White)))
            .border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(widget, area);
}

fn render_preview(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let pos = app.nav.position();

    if let Some(content) = app.lattice.read_scroll(&pos) {
        // Add line numbers
        let lines: Vec<Line> = content.lines().enumerate().map(|(i, line)| {
            Line::from(vec![
                Span::styled(format!("{:>4} ", i + 1), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("│ {}", line), Style::default().fg(Color::White)),
            ])
        }).collect();

        let title = format!(" {} — {} ", pos,
            app.current_stats().map(|s| s.size_display()).unwrap_or_default());

        let widget = Paragraph::new(Text::from(lines))
            .block(Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(title, Style::default().fg(Color::Green)))
                .border_style(Style::default().fg(Color::DarkGray)))
            .wrap(Wrap { trim: false });
        f.render_widget(widget, area);
    } else {
        let widget = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled("  ○ empty coordinate", Style::default().fg(Color::DarkGray))),
            Line::from(""),
            Line::from(Span::styled("  Navigate to a populated scroll", Style::default().fg(Color::DarkGray))),
            Line::from(Span::styled("  with j/k or goto with g", Style::default().fg(Color::DarkGray))),
        ])
        .block(Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(format!(" {} ", pos), Style::default().fg(Color::DarkGray)))
            .border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(widget, area);
    }
}

fn render_scroll(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let pos = app.nav.position();
    let content = app.lattice.read_scroll(&pos).unwrap_or_default();
    let stats = app.current_stats();
    let total_lines = content.lines().count();

    // Line-numbered content
    let lines: Vec<Line> = content.lines().enumerate().map(|(i, line)| {
        Line::from(vec![
            Span::styled(format!("{:>5} ", i + 1), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("│ {}", line), Style::default().fg(Color::White)),
        ])
    }).collect();

    let stats_str = stats.map(|s| format!("  {} │ {} lines │ {} words │ {}",
        pos, s.line_count, s.word_count, s.size_display()
    )).unwrap_or_else(|| format!("  {}", pos));

    let scroll_info = if total_lines > 0 {
        let visible_start = (app.scroll_offset as usize + 1).min(total_lines);
        format!(" {}/{} ", visible_start, total_lines)
    } else {
        String::new()
    };

    let widget = Paragraph::new(Text::from(lines))
        .block(Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(stats_str, Style::default().fg(Color::Green)))
            .title_bottom(Span::styled(scroll_info, Style::default().fg(Color::DarkGray)))
            .border_style(Style::default().fg(Color::DarkGray)))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));
    f.render_widget(widget, area);
}

fn render_input(f: &mut ratatui::Frame, prompt: &str, input: &str, hint: &str, area: Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {} ", prompt), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(input, Style::default().fg(Color::White)),
            Span::styled("▌", Style::default().fg(Color::Yellow)),
        ]),
        Line::from(""),
        Line::from(Span::styled(format!("  {}", hint), Style::default().fg(Color::DarkGray))),
    ];
    let widget = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(widget, area);
}

fn render_search_results(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app.search_hits.iter().enumerate().map(|(i, hit)| {
        let is_selected = i == app.search_cursor;
        let marker = if is_selected { "▸" } else { " " };
        let coord_style = if is_selected {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };
        let ctx_style = if is_selected {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::Gray)
        };

        let context = hit.context.replace('\n', " ");
        let context: String = context.chars().take(80).collect();

        ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", marker), coord_style),
            Span::styled(format!("{} ", hit.coordinate), coord_style),
            Span::styled(context, ctx_style),
        ]))
    }).collect();

    let title = format!(" {} results ", app.search_hits.len());
    let position = format!(" {}/{} ", app.search_cursor + 1, app.search_hits.len());

    let widget = List::new(items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(title, Style::default().fg(Color::Yellow)))
            .title_bottom(Span::styled(position, Style::default().fg(Color::DarkGray)))
            .border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(widget, area);
}

fn render_status_bar(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let mode_label = match app.mode {
        Mode::Lattice => "LATTICE",
        Mode::ScrollView => "SCROLL",
        Mode::GotoInput => "GOTO",
        Mode::SearchInput => "SEARCH",
        Mode::SearchResults => "RESULTS",
        Mode::Help => "HELP",
        Mode::Info => "INFO",
    };

    let help_hint = match app.mode {
        Mode::Lattice => "1-9:dim  h/l:move  j/k:nav  Enter:view  /:search  g:goto  ?:help",
        Mode::ScrollView => "j/k:scroll  d/u:page  n/p:next/prev scroll  g/G:top/bottom  Esc:back",
        Mode::GotoInput => "Enter:jump  Esc:cancel",
        Mode::SearchInput => "Enter:search  Esc:cancel",
        Mode::SearchResults => "j/k:navigate  Enter:jump to coordinate  Esc:back",
        Mode::Help | Mode::Info => "Esc/q:close",
    };

    let status_text = if !app.status_message.is_empty() {
        &app.status_message
    } else {
        help_hint
    };

    let line = Line::from(vec![
        Span::styled(format!(" {} ", mode_label),
            Style::default().fg(Color::Black).bg(match app.mode {
                Mode::Lattice => Color::Cyan,
                Mode::ScrollView => Color::Green,
                Mode::SearchResults => Color::Yellow,
                _ => Color::DarkGray,
            })),
        Span::styled(format!(" {} ", status_text), Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_help_overlay(f: &mut ratatui::Frame, area: Rect) {
    let overlay = centered_rect(60, 80, area);
    f.render_widget(Clear, overlay);

    let help_text = vec![
        Line::from(Span::styled(" Lattice Navigation", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![Span::styled("  1-9        ", Style::default().fg(Color::Yellow)), Span::raw("Select dimension (Scroll → Library)")]),
        Line::from(vec![Span::styled("  Tab/S-Tab  ", Style::default().fg(Color::Yellow)), Span::raw("Cycle dimension group (Z/Y/X)")]),
        Line::from(vec![Span::styled("  h / l      ", Style::default().fg(Color::Yellow)), Span::raw("Move backward / forward in dimension")]),
        Line::from(vec![Span::styled("  j / k      ", Style::default().fg(Color::Yellow)), Span::raw("Next / previous populated scroll")]),
        Line::from(vec![Span::styled("  J / K      ", Style::default().fg(Color::Yellow)), Span::raw("Jump 10 populated scrolls")]),
        Line::from(vec![Span::styled("  g          ", Style::default().fg(Color::Yellow)), Span::raw("Goto coordinate")]),
        Line::from(vec![Span::styled("  Home       ", Style::default().fg(Color::Yellow)), Span::raw("Jump to BASE (1.1.1/1.1.1/1.1.1)")]),
        Line::from(""),
        Line::from(Span::styled(" Scroll Viewing", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![Span::styled("  Enter      ", Style::default().fg(Color::Yellow)), Span::raw("Open scroll at current coordinate")]),
        Line::from(vec![Span::styled("  Space      ", Style::default().fg(Color::Yellow)), Span::raw("Toggle inline preview panel")]),
        Line::from(vec![Span::styled("  n / p      ", Style::default().fg(Color::Yellow)), Span::raw("Next / previous scroll (in view)")]),
        Line::from(vec![Span::styled("  j/k d/u    ", Style::default().fg(Color::Yellow)), Span::raw("Scroll / page through content")]),
        Line::from(""),
        Line::from(Span::styled(" Search & Jump", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![Span::styled("  /          ", Style::default().fg(Color::Yellow)), Span::raw("Search across all scrolls")]),
        Line::from(vec![Span::styled("  m          ", Style::default().fg(Color::Yellow)), Span::raw("Set mark at current position")]),
        Line::from(vec![Span::styled("  '          ", Style::default().fg(Color::Yellow)), Span::raw("Jump to last mark")]),
        Line::from(vec![Span::styled("  [ / ]      ", Style::default().fg(Color::Yellow)), Span::raw("History back / forward")]),
        Line::from(""),
        Line::from(vec![Span::styled("  ? ", Style::default().fg(Color::Yellow)), Span::raw("this help"), Span::styled("  i ", Style::default().fg(Color::Yellow)), Span::raw("lattice info"), Span::styled("  q ", Style::default().fg(Color::Yellow)), Span::raw("quit")]),
    ];

    let widget = Paragraph::new(Text::from(help_text))
        .block(Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" 💎 phext-nav help ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
            .border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(widget, overlay);
}

fn render_info_overlay(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let overlay = centered_rect(60, 70, area);
    f.render_widget(Clear, overlay);

    let ov = &app.overview;
    let mut lines = vec![
        Line::from(Span::styled(" Lattice Overview", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![
            Span::styled("  File:     ", Style::default().fg(Color::Gray)),
            Span::styled(&app.file_path, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Size:     ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:.2} MB ({} bytes)", ov.total_bytes as f64 / 1_048_576.0, ov.total_bytes), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Scrolls:  ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{}", ov.total_scrolls), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Avg size: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{} bytes", ov.avg_scroll_size), Style::default().fg(Color::White)),
        ]),
    ];

    if let Some((coord, size)) = &ov.largest_scroll {
        lines.push(Line::from(vec![
            Span::styled("  Largest:  ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{} ({})", coord, ScrollStats::from_bytes(&vec![0; *size]).size_display()), Style::default().fg(Color::White)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" Dimension Distribution", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))));
    lines.push(Line::from(""));

    for density in &ov.dimension_densities {
        let spark = density.sparkline(20);
        let range = density.range().map(|(lo, hi)| format!("{}-{}", lo, hi)).unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<12}", density.dimension.name()), Style::default().fg(Color::Gray)),
            Span::styled(format!("{:>3} vals ", density.distinct_values()), Style::default().fg(Color::DarkGray)),
            Span::styled(spark, Style::default().fg(Color::Cyan)),
            Span::styled(format!(" {}", range), Style::default().fg(Color::DarkGray)),
        ]));
    }

    let widget = Paragraph::new(Text::from(lines))
        .block(Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" 💎 lattice info ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
            .border_style(Style::default().fg(Color::Magenta)));
    f.render_widget(widget, overlay);
}

/// Create a centered rect within `area` as a percentage of its size.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
