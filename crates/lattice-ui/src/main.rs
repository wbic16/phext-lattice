/// phext-edit — GPU-rendered phext editor.
///
/// The body of Zed (Rust + gpui), the soul of Emacs (extensible runtime),
/// the mind of Helix (modal, structural, intelligent).

mod coordinate_bar;
mod dimension_panel;
mod scroll_view;
mod editor_pane;
mod theme;

use gpui::*;
use lattice_core::{
    MappedLattice, Navigator, Dimension, CoordinateNav,
    Sentron, VimMode, EditorMode,
    UndoEngine, LatticeOverview, DimensionDensity,
};

/// The root window view — owns all editor state.
struct PhextWindow {
    lattice: MappedLattice,
    nav: Navigator,
    sentron: Sentron,
    overview: LatticeOverview,
    _densities: Vec<DimensionDensity>,
    _undo: UndoEngine,
    _mode: Box<dyn EditorMode>,
    file_path: String,
    focus_handle: FocusHandle,
}

impl PhextWindow {
    fn new(lattice: MappedLattice, file_path: String, window: &mut Window, cx: &mut App) -> Self {
        let buf = lattice.to_phext_bytes();
        let overview = LatticeOverview::build(&buf, lattice.index());
        let densities: Vec<DimensionDensity> = (1..=9u8)
            .map(|i| DimensionDensity::build(Dimension::from_index(i).unwrap(), lattice.index()))
            .collect();

        let mut nav = Navigator::new();
        if lattice.has_scroll(&libphext::phext::default_coordinate()) {
            nav.goto(libphext::phext::default_coordinate());
        } else {
            nav.next_populated(lattice.index());
        }

        let sentron = Sentron::build(&nav.position(), lattice.index());
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);

        PhextWindow {
            lattice,
            nav,
            sentron,
            overview,
            _densities: densities,
            _undo: UndoEngine::new(),
            _mode: Box::new(VimMode::new()),
            file_path,
            focus_handle,
        }
    }

    fn refresh_sentron(&mut self) {
        if self.sentron.center != self.nav.position() {
            self.sentron = Sentron::build(&self.nav.position(), self.lattice.index());
        }
    }

    fn coordinate_text(&self) -> String {
        let pos = self.nav.position();
        let dim = self.nav.active_dimension();
        let has = if self.lattice.has_scroll(&pos) { "●" } else { "○" };
        format!("💎 {} ◆ {} {} [{} scrolls]",
            pos, dim.name(), has, self.overview.total_scrolls)
    }

    fn sentron_text(&self) -> String {
        let s = &self.sentron;
        let mut out = format!("◉ sentron [{}/40]\nreach: {}↔{}\n\n",
            s.size(), s.structural_reach, s.sequential_reach);

        out.push_str("── spatial ──\n");
        for axon in s.structural_axons() {
            out.push_str(&format!("  {} {:<10}  −{:>3}  +{:<3}\n",
                axon.dimension as u8,
                axon.dimension.name(),
                axon.backward_count,
                axon.forward_count));
        }

        out.push_str("\n── temporal ──\n");
        for axon in s.sequential_axons() {
            out.push_str(&format!("  {} {:<10}  −{:>3}  +{:<3}\n",
                axon.dimension as u8,
                axon.dimension.name(),
                axon.backward_count,
                axon.forward_count));
        }

        let pos = self.nav.position();
        out.push_str(&format!("\n  9 {:<10}  = {}\n",
            "Scroll", pos.dimension_value(Dimension::Scroll)));

        out
    }

    fn content_preview(&self) -> String {
        let pos = self.nav.position();
        self.lattice.read_scroll(&pos)
            .unwrap_or_else(|| "○ empty coordinate\n\nNavigate with j/k or goto with g".to_string())
    }
}

impl Render for PhextWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_sentron();

        let coord_text: SharedString = self.coordinate_text().into();
        let sentron_text: SharedString = self.sentron_text().into();
        let content: SharedString = self.content_preview().into();
        let status: SharedString = "LATTICE  1-9:dim  h/l:move  j/k:nav  Enter:edit  /:search  q:quit".into();

        div()
            .id("phext-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .text_sm()
            // Coordinate bar
            .child(
                div()
                    .h(px(36.0))
                    .w_full()
                    .bg(theme::bar_bg())
                    .border_b_1()
                    .border_color(theme::border())
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .px_3()
                            .text_color(theme::coord_text())
                            .child(coord_text)
                    )
            )
            // Main area: sentron panel + scroll content
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    // Left: Sentron panel
                    .child(
                        div()
                            .w(px(300.0))
                            .h_full()
                            .bg(theme::panel_bg())
                            .border_r_1()
                            .border_color(theme::border())
                            .overflow_hidden()
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .text_xs()
                                    .text_color(theme::dim_text())
                                    .child(sentron_text)
                            )
                    )
                    // Right: Scroll content
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
                            .child(
                                div()
                                    .px_4()
                                    .py_3()
                                    .text_color(theme::text())
                                    .child(content)
                            )
                    )
            )
            // Status bar
            .child(
                div()
                    .h(px(24.0))
                    .w_full()
                    .bg(theme::status_bg())
                    .border_t_1()
                    .border_color(theme::border())
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .px_3()
                            .text_xs()
                            .text_color(theme::dim_text())
                            .child(status)
                    )
            )
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("phext-edit — GPU-rendered phext editor");
        eprintln!();
        eprintln!("Usage: phext-edit <file.phext>");
        std::process::exit(1);
    }

    let file_path = args[1].clone();
    let lattice = MappedLattice::open(&file_path).unwrap_or_else(|e| {
        eprintln!("Failed to open {}: {}", file_path, e);
        std::process::exit(1);
    });

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(format!("💎 phext-edit — {}", file_path).into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| PhextWindow::new(lattice, file_path.clone(), window, cx)),
        ).unwrap();
    });
}
