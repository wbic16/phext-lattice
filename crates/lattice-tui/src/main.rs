/// phext-nav — Terminal UI for navigating phext lattices.
///
/// This is the Shell of Nine interaction model in a terminal,
/// validating the Navigate → Orient → Edit grammar before gpui.
///
/// Keys (Lattice mode):
///   1-9       Select dimension
///   h/l       Move backward/forward in active dimension
///   j/k       Previous/next populated coordinate
///   g         Goto coordinate (opens input)
///   /         Search across lattice
///   Enter     View scroll content at current coordinate
///   m         Set mark
///   '         Jump to mark
///   [/]       History back/forward
///   q         Quit

use std::io;
use std::env;
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
    widgets::{Block, Borders, Paragraph, Wrap, List, ListItem},
    Terminal,
};
use lattice_core::{
    MappedLattice, Navigator, Dimension, CoordinateNav,
    search_lattice, SearchHit,
};
use libphext::phext::to_coordinate;

#[derive(Debug, PartialEq)]
enum Mode {
    Lattice,
    ScrollView,
    GotoInput,
    SearchInput,
    SearchResults,
}

struct App {
    lattice: MappedLattice,
    nav: Navigator,
    mode: Mode,
    input_buffer: String,
    search_hits: Vec<SearchHit>,
    search_cursor: usize,
    status_message: String,
    scroll_offset: u16,
}

impl App {
    fn new(lattice: MappedLattice) -> App {
        let mut nav = Navigator::new();
        // Jump to first populated coordinate
        nav.next_populated(lattice.index());
        // Go back to BASE if it has content, otherwise stay
        if lattice.has_scroll(&libphext::phext::default_coordinate()) {
            nav.goto(libphext::phext::default_coordinate());
        }

        App {
            lattice,
            nav,
            mode: Mode::Lattice,
            input_buffer: String::new(),
            search_hits: Vec::new(),
            search_cursor: 0,
            status_message: String::new(),
            scroll_offset: 0,
        }
    }

    fn status_line(&self) -> String {
        let pos = self.nav.position();
        let dim = self.nav.active_dimension();
        let (prev, _current, next) = self.nav.dimension_neighbors();
        let has_content = self.lattice.has_scroll(&pos);
        let total = self.lattice.index().scroll_count();

        format!(
            " {} [dim:{} {}]  ← {} | {} →  [{} scrolls]{}",
            pos,
            dim as u8,
            dim.name(),
            prev.map(|v| v.to_string()).unwrap_or_else(|| "—".to_string()),
            next.map(|v| v.to_string()).unwrap_or_else(|| "—".to_string()),
            total,
            if has_content { " ●" } else { " ○" },
        )
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: phext-nav <file.phext>");
        eprintln!();
        eprintln!("Navigate a phext lattice in the terminal.");
        eprintln!("Keys: 1-9 (dimension), h/l (move), j/k (populated), Enter (view), / (search), q (quit)");
        std::process::exit(1);
    }

    let lattice = MappedLattice::open(&args[1]).map_err(|e| {
        eprintln!("Failed to open {}: {}", args[1], e);
        e
    })?;

    let mut app = App::new(lattice);

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Main loop
    loop {
        terminal.draw(|f| ui(f, &app))?;

        if let Event::Key(key) = event::read()? {
            // Global: Ctrl-C always quits
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break;
            }

            match app.mode {
                Mode::Lattice => match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('1') => { app.nav.select_dimension(1); }
                    KeyCode::Char('2') => { app.nav.select_dimension(2); }
                    KeyCode::Char('3') => { app.nav.select_dimension(3); }
                    KeyCode::Char('4') => { app.nav.select_dimension(4); }
                    KeyCode::Char('5') => { app.nav.select_dimension(5); }
                    KeyCode::Char('6') => { app.nav.select_dimension(6); }
                    KeyCode::Char('7') => { app.nav.select_dimension(7); }
                    KeyCode::Char('8') => { app.nav.select_dimension(8); }
                    KeyCode::Char('9') => { app.nav.select_dimension(9); }
                    KeyCode::Char('l') | KeyCode::Right => {
                        if !app.nav.move_forward() {
                            app.status_message = "At maximum".to_string();
                        }
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        if !app.nav.move_backward() {
                            app.status_message = "At minimum".to_string();
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if !app.nav.next_populated(app.lattice.index()) {
                            app.status_message = "No more populated scrolls".to_string();
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if !app.nav.prev_populated(app.lattice.index()) {
                            app.status_message = "At first populated scroll".to_string();
                        }
                    }
                    KeyCode::Enter => {
                        if app.lattice.has_scroll(&app.nav.position()) {
                            app.mode = Mode::ScrollView;
                            app.scroll_offset = 0;
                        } else {
                            app.status_message = "Empty coordinate".to_string();
                        }
                    }
                    KeyCode::Char('g') => {
                        app.mode = Mode::GotoInput;
                        app.input_buffer.clear();
                    }
                    KeyCode::Char('/') => {
                        app.mode = Mode::SearchInput;
                        app.input_buffer.clear();
                    }
                    KeyCode::Char('m') => {
                        app.nav.set_mark();
                        app.status_message = "Mark set".to_string();
                    }
                    KeyCode::Char('\'') => {
                        if !app.nav.jump_to_mark() {
                            app.status_message = "No marks".to_string();
                        }
                    }
                    KeyCode::Char('[') => {
                        if !app.nav.history_back() {
                            app.status_message = "No history".to_string();
                        }
                    }
                    KeyCode::Char(']') => {
                        if !app.nav.history_forward() {
                            app.status_message = "At latest".to_string();
                        }
                    }
                    _ => {}
                },

                Mode::ScrollView => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        app.mode = Mode::Lattice;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.scroll_offset = app.scroll_offset.saturating_add(1);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.scroll_offset = app.scroll_offset.saturating_sub(1);
                    }
                    KeyCode::PageDown => {
                        app.scroll_offset = app.scroll_offset.saturating_add(20);
                    }
                    KeyCode::PageUp => {
                        app.scroll_offset = app.scroll_offset.saturating_sub(20);
                    }
                    _ => {}
                },

                Mode::GotoInput => match key.code {
                    KeyCode::Esc => {
                        app.mode = Mode::Lattice;
                    }
                    KeyCode::Enter => {
                        let coord = to_coordinate(&app.input_buffer);
                        app.nav.goto(coord);
                        app.mode = Mode::Lattice;
                        app.status_message = format!("Jumped to {}", coord);
                    }
                    KeyCode::Char(c) => {
                        app.input_buffer.push(c);
                    }
                    KeyCode::Backspace => {
                        app.input_buffer.pop();
                    }
                    _ => {}
                },

                Mode::SearchInput => match key.code {
                    KeyCode::Esc => {
                        app.mode = Mode::Lattice;
                    }
                    KeyCode::Enter => {
                        let pattern = app.input_buffer.clone();
                        let buf;
                        let buffer: &[u8] = if let Some(raw) = app.lattice.raw_buffer() {
                            raw
                        } else {
                            buf = app.lattice.to_phext_bytes();
                            &buf
                        };
                        let hits = search_lattice(
                            buffer,
                            app.lattice.index(),
                            &pattern,
                            false,
                            100,
                        );
                        app.status_message = format!("{} hits for \"{}\"", hits.len(), pattern);
                        if hits.is_empty() {
                            app.mode = Mode::Lattice;
                        } else {
                            app.search_hits = hits;
                            app.search_cursor = 0;
                            app.mode = Mode::SearchResults;
                        }
                    }
                    KeyCode::Char(c) => {
                        app.input_buffer.push(c);
                    }
                    KeyCode::Backspace => {
                        app.input_buffer.pop();
                    }
                    _ => {}
                },

                Mode::SearchResults => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        app.mode = Mode::Lattice;
                    }
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
                },
            }

            // Clear status after any key in lattice mode (except the one that just set it)
            if app.mode == Mode::Lattice && !app.status_message.is_empty() {
                // We'll clear it on the NEXT keystroke
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

fn ui(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Coordinate bar
            Constraint::Min(1),    // Main content
            Constraint::Length(1), // Status / help
        ])
        .split(f.area());

    // Coordinate bar
    let coord_text = app.status_line();
    let coord_bar = Paragraph::new(coord_text)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(" 💎 phext-nav "))
        .style(Style::default().fg(Color::Yellow));
    f.render_widget(coord_bar, chunks[0]);

    // Main content
    match &app.mode {
        Mode::Lattice => {
            render_lattice_view(f, app, chunks[1]);
        }
        Mode::ScrollView => {
            render_scroll_view(f, app, chunks[1]);
        }
        Mode::GotoInput => {
            render_input(f, "Goto coordinate:", &app.input_buffer, chunks[1]);
        }
        Mode::SearchInput => {
            render_input(f, "Search:", &app.input_buffer, chunks[1]);
        }
        Mode::SearchResults => {
            render_search_results(f, app, chunks[1]);
        }
    }

    // Status line
    let help = match app.mode {
        Mode::Lattice => {
            if app.status_message.is_empty() {
                "1-9:dim  h/l:move  j/k:populated  Enter:view  g:goto  /:search  m:mark  ':jump  q:quit"
            } else {
                &app.status_message
            }
        }
        Mode::ScrollView => "j/k:scroll  PgUp/PgDn  Esc:back",
        Mode::GotoInput => "Enter:jump  Esc:cancel",
        Mode::SearchInput => "Enter:search  Esc:cancel",
        Mode::SearchResults => "j/k:navigate  Enter:jump  Esc:back",
    };
    let status = Paragraph::new(help)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(status, chunks[2]);
}

fn render_lattice_view(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let pos = app.nav.position();
    let summary = app.nav.summarize(app.lattice.index());

    let mut lines: Vec<Line> = Vec::new();

    // Current position info
    lines.push(Line::from(vec![
        Span::styled("  Position: ", Style::default().fg(Color::Gray)),
        Span::styled(format!("{}", pos), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(""));

    // Active dimension
    let dim = app.nav.active_dimension();
    lines.push(Line::from(vec![
        Span::styled("  Dimension: ", Style::default().fg(Color::Gray)),
        Span::styled(format!("{} ({})", dim.name(), dim as u8), Style::default().fg(Color::Cyan)),
    ]));
    lines.push(Line::from(""));

    // Content preview
    if let Some(content) = app.lattice.read_scroll(&pos) {
        let preview: String = content.chars().take(200).collect();
        lines.push(Line::from(vec![
            Span::styled("  Content: ", Style::default().fg(Color::Gray)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", preview), Style::default().fg(Color::Green)),
        ]));
        if content.len() > 200 {
            lines.push(Line::from(vec![
                Span::styled(format!("  ... ({} bytes total)", content.len()), Style::default().fg(Color::DarkGray)),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("  (empty coordinate)", Style::default().fg(Color::DarkGray)),
        ]));
    }

    lines.push(Line::from(""));

    // Neighbors
    if let Some(next) = summary.next {
        lines.push(Line::from(vec![
            Span::styled("  Next: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{}", next), Style::default().fg(Color::Blue)),
        ]));
    }
    if let Some(prev) = summary.prev {
        lines.push(Line::from(vec![
            Span::styled("  Prev: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{}", prev), Style::default().fg(Color::Blue)),
        ]));
    }

    // Dimension guide
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Dimensions: ", Style::default().fg(Color::DarkGray)),
    ]));
    for i in 1..=9u8 {
        let d = Dimension::from_index(i).unwrap();
        let style = if d == dim {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let val = pos.dimension_value(d);
        lines.push(Line::from(vec![
            Span::styled(format!("    {} {} = {}", i, d.name(), val), style),
        ]));
    }

    let content = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title(" Lattice "));
    f.render_widget(content, area);
}

fn render_scroll_view(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let pos = app.nav.position();
    let content = app.lattice.read_scroll(&pos).unwrap_or_default();

    let paragraph = Paragraph::new(content.as_str())
        .block(Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", pos)))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));
    f.render_widget(paragraph, area);
}

fn render_input(f: &mut ratatui::Frame, prompt: &str, input: &str, area: Rect) {
    let text = format!("{} {}_", prompt, input);
    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(paragraph, area);
}

fn render_search_results(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app.search_hits.iter().enumerate().map(|(i, hit)| {
        let style = if i == app.search_cursor {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let text = format!("{} — {}", hit.coordinate, hit.context.replace('\n', " "));
        ListItem::new(text).style(style)
    }).collect();

    let list = List::new(items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(format!(" Search: {} results ", app.search_hits.len())));
    f.render_widget(list, area);
}
