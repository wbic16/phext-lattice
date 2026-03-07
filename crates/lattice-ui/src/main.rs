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

// ── Actions ────────────────────────────────────────────────────────────

actions!(phext_edit, [
    // Dimension selection
    Dim1, Dim2, Dim3, Dim4, Dim5, Dim6, Dim7, Dim8, Dim9,
    // Movement
    MoveForward, MoveBackward,
    NextPopulated, PrevPopulated,
    JumpForward10, JumpBackward10,
    // Navigation
    CycleDimGroupForward, CycleDimGroupBackward,
    JumpToBase,
    // View
    EnterScroll, ExitScroll,
    TogglePreview,
    // Quit
    Quit,
]);

// ── Window ─────────────────────────────────────────────────────────────

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
    viewing_scroll: bool,
    scroll_offset: f32,
    status_msg: String,
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
            viewing_scroll: false,
            scroll_offset: 0.0,
            status_msg: String::new(),
        }
    }

    fn refresh_sentron(&mut self) {
        if self.sentron.center != self.nav.position() {
            self.sentron = Sentron::build(&self.nav.position(), self.lattice.index());
        }
    }

    // ── Action handlers ──

    fn on_dim(&mut self, dim: u8, _window: &mut Window, cx: &mut Context<Self>) {
        self.nav.select_dimension(dim);
        cx.notify();
    }

    fn on_move_forward(&mut self, _: &MoveForward, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.nav.move_forward() {
            self.status_msg = "▸ boundary".into();
        }
        cx.notify();
    }

    fn on_move_backward(&mut self, _: &MoveBackward, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.nav.move_backward() {
            self.status_msg = "◂ boundary".into();
        }
        cx.notify();
    }

    fn on_next_populated(&mut self, _: &NextPopulated, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.nav.next_populated(self.lattice.index()) {
            self.status_msg = "▾ last scroll".into();
        }
        cx.notify();
    }

    fn on_prev_populated(&mut self, _: &PrevPopulated, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.nav.prev_populated(self.lattice.index()) {
            self.status_msg = "▴ first scroll".into();
        }
        cx.notify();
    }

    fn on_jump_forward_10(&mut self, _: &JumpForward10, _window: &mut Window, cx: &mut Context<Self>) {
        let mut n = 0;
        for _ in 0..10 {
            if self.nav.next_populated(self.lattice.index()) { n += 1; } else { break; }
        }
        if n > 0 { self.status_msg = format!("↓{}", n); }
        cx.notify();
    }

    fn on_jump_backward_10(&mut self, _: &JumpBackward10, _window: &mut Window, cx: &mut Context<Self>) {
        let mut n = 0;
        for _ in 0..10 {
            if self.nav.prev_populated(self.lattice.index()) { n += 1; } else { break; }
        }
        if n > 0 { self.status_msg = format!("↑{}", n); }
        cx.notify();
    }

    fn on_cycle_dim_forward(&mut self, _: &CycleDimGroupForward, _window: &mut Window, cx: &mut Context<Self>) {
        let dim = self.nav.active_dimension() as u8;
        let next = if dim <= 3 { 4 } else if dim <= 6 { 7 } else { 1 };
        self.nav.select_dimension(next);
        cx.notify();
    }

    fn on_cycle_dim_backward(&mut self, _: &CycleDimGroupBackward, _window: &mut Window, cx: &mut Context<Self>) {
        let dim = self.nav.active_dimension() as u8;
        let prev = if dim >= 7 { 4 } else if dim >= 4 { 1 } else { 7 };
        self.nav.select_dimension(prev);
        cx.notify();
    }

    fn on_jump_to_base(&mut self, _: &JumpToBase, _window: &mut Window, cx: &mut Context<Self>) {
        self.nav.goto(libphext::phext::default_coordinate());
        self.status_msg = "⌂ BASE".into();
        cx.notify();
    }

    fn on_enter_scroll(&mut self, _: &EnterScroll, _window: &mut Window, cx: &mut Context<Self>) {
        if self.lattice.has_scroll(&self.nav.position()) {
            self.viewing_scroll = true;
            self.scroll_offset = 0.0;
        } else {
            self.status_msg = "○ empty coordinate".into();
        }
        cx.notify();
    }

    fn on_exit_scroll(&mut self, _: &ExitScroll, _window: &mut Window, cx: &mut Context<Self>) {
        self.viewing_scroll = false;
        cx.notify();
    }

    fn on_quit(&mut self, _: &Quit, _window: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    // ── Rendering helpers ──

    fn coordinate_text(&self) -> String {
        let pos = self.nav.position();
        let dim = self.nav.active_dimension();
        let has = if self.lattice.has_scroll(&pos) { "●" } else { "○" };
        format!("💎 {} ◆ {} {} [{} scrolls]",
            pos, dim.name(), has, self.overview.total_scrolls)
    }

    fn sentron_lines(&self) -> Vec<(String, theme::ArmColor)> {
        let s = &self.sentron;
        let pos = self.nav.position();
        let dim = self.nav.active_dimension();
        let mut lines = Vec::new();

        lines.push((format!("◉ sentron [{}/40]  {}↔{}", s.size(), s.structural_reach, s.sequential_reach), theme::ArmColor::Neutral));
        lines.push((String::new(), theme::ArmColor::Neutral));

        lines.push(("── spatial ──".into(), theme::ArmColor::Z));
        for axon in s.structural_axons() {
            let marker = if axon.dimension == dim { "▸" } else { " " };
            lines.push((
                format!("{}{} {:<10} {:>4}  −{:>3} +{:<3}", marker, axon.dimension as u8, axon.dimension.name(), pos.dimension_value(axon.dimension), axon.backward_count, axon.forward_count),
                theme::ArmColor::Z,
            ));
        }
        lines.push((String::new(), theme::ArmColor::Neutral));

        lines.push(("── temporal ──".into(), theme::ArmColor::Y));
        for axon in s.sequential_axons() {
            let marker = if axon.dimension == dim { "▸" } else { " " };
            lines.push((
                format!("{}{} {:<10} {:>4}  −{:>3} +{:<3}", marker, axon.dimension as u8, axon.dimension.name(), pos.dimension_value(axon.dimension), axon.backward_count, axon.forward_count),
                theme::ArmColor::Y,
            ));
        }
        lines.push((String::new(), theme::ArmColor::Neutral));

        let scroll_marker = if dim == Dimension::Scroll { "▸" } else { " " };
        lines.push((
            format!("{}9 {:<10} {:>4}  ← neuron", scroll_marker, "Scroll", pos.dimension_value(Dimension::Scroll)),
            theme::ArmColor::X,
        ));

        lines
    }

    fn content_preview(&self) -> String {
        let pos = self.nav.position();
        self.lattice.read_scroll(&pos)
            .unwrap_or_else(|| "○ empty coordinate\n\nNavigate to a populated scroll\nwith j/k or goto with g".to_string())
    }

    fn mode_label(&self) -> String {
        if self.viewing_scroll { "SCROLL".into() } else { "LATTICE".into() }
    }

    fn help_text(&self) -> String {
        if self.viewing_scroll {
            "Esc:back to lattice".into()
        } else if !self.status_msg.is_empty() {
            self.status_msg.clone()
        } else {
            "1-9:dim  h/l:move  j/k:nav  J/K:×10  Enter:view  Tab:cycle  Home:BASE  q:quit".into()
        }
    }
}

impl Render for PhextWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_sentron();

        let coord_text: SharedString = self.coordinate_text().into();
        let content: SharedString = self.content_preview().into();
        let mode: SharedString = self.mode_label().into();
        let help: SharedString = self.help_text().into();

        // Build sentron panel lines
        let sentron_lines = self.sentron_lines();
        let sentron_children: Vec<Div> = sentron_lines.into_iter().map(|(text, color)| {
            let text: SharedString = text.into();
            div()
                .text_color(color.to_hsla())
                .child(text)
        }).collect();

        div()
            .id("phext-root")
            .key_context("PhextEdit")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_move_forward))
            .on_action(cx.listener(Self::on_move_backward))
            .on_action(cx.listener(Self::on_next_populated))
            .on_action(cx.listener(Self::on_prev_populated))
            .on_action(cx.listener(Self::on_jump_forward_10))
            .on_action(cx.listener(Self::on_jump_backward_10))
            .on_action(cx.listener(Self::on_cycle_dim_forward))
            .on_action(cx.listener(Self::on_cycle_dim_backward))
            .on_action(cx.listener(Self::on_jump_to_base))
            .on_action(cx.listener(Self::on_enter_scroll))
            .on_action(cx.listener(Self::on_exit_scroll))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(|this: &mut Self, _: &Dim1, _w, cx| { this.on_dim(1, _w, cx); }))
            .on_action(cx.listener(|this: &mut Self, _: &Dim2, _w, cx| { this.on_dim(2, _w, cx); }))
            .on_action(cx.listener(|this: &mut Self, _: &Dim3, _w, cx| { this.on_dim(3, _w, cx); }))
            .on_action(cx.listener(|this: &mut Self, _: &Dim4, _w, cx| { this.on_dim(4, _w, cx); }))
            .on_action(cx.listener(|this: &mut Self, _: &Dim5, _w, cx| { this.on_dim(5, _w, cx); }))
            .on_action(cx.listener(|this: &mut Self, _: &Dim6, _w, cx| { this.on_dim(6, _w, cx); }))
            .on_action(cx.listener(|this: &mut Self, _: &Dim7, _w, cx| { this.on_dim(7, _w, cx); }))
            .on_action(cx.listener(|this: &mut Self, _: &Dim8, _w, cx| { this.on_dim(8, _w, cx); }))
            .on_action(cx.listener(|this: &mut Self, _: &Dim9, _w, cx| { this.on_dim(9, _w, cx); }))
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
                    .child(div().px_3().text_color(theme::coord_text()).child(coord_text))
            )
            // Main area
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    // Left: Sentron panel
                    .child(
                        div()
                            .w(px(320.0))
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
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .children(sentron_children)
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
                            .px_2()
                            .mx_1()
                            .text_xs()
                            .bg(theme::z_color())
                            .text_color(gpui::black())
                            .child(mode)
                    )
                    .child(
                        div()
                            .px_3()
                            .text_xs()
                            .text_color(theme::dim_text())
                            .child(help)
                    )
            )
    }
}

// ── Main ───────────────────────────────────────────────────────────────

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
        // Key bindings — Lattice mode
        cx.bind_keys([
            KeyBinding::new("1", Dim1, Some("PhextEdit")),
            KeyBinding::new("2", Dim2, Some("PhextEdit")),
            KeyBinding::new("3", Dim3, Some("PhextEdit")),
            KeyBinding::new("4", Dim4, Some("PhextEdit")),
            KeyBinding::new("5", Dim5, Some("PhextEdit")),
            KeyBinding::new("6", Dim6, Some("PhextEdit")),
            KeyBinding::new("7", Dim7, Some("PhextEdit")),
            KeyBinding::new("8", Dim8, Some("PhextEdit")),
            KeyBinding::new("9", Dim9, Some("PhextEdit")),
            KeyBinding::new("l", MoveForward, Some("PhextEdit")),
            KeyBinding::new("right", MoveForward, Some("PhextEdit")),
            KeyBinding::new("h", MoveBackward, Some("PhextEdit")),
            KeyBinding::new("left", MoveBackward, Some("PhextEdit")),
            KeyBinding::new("j", NextPopulated, Some("PhextEdit")),
            KeyBinding::new("down", NextPopulated, Some("PhextEdit")),
            KeyBinding::new("k", PrevPopulated, Some("PhextEdit")),
            KeyBinding::new("up", PrevPopulated, Some("PhextEdit")),
            KeyBinding::new("shift-j", JumpForward10, Some("PhextEdit")),
            KeyBinding::new("shift-k", JumpBackward10, Some("PhextEdit")),
            KeyBinding::new("tab", CycleDimGroupForward, Some("PhextEdit")),
            KeyBinding::new("shift-tab", CycleDimGroupBackward, Some("PhextEdit")),
            KeyBinding::new("home", JumpToBase, Some("PhextEdit")),
            KeyBinding::new("enter", EnterScroll, Some("PhextEdit")),
            KeyBinding::new("escape", ExitScroll, Some("PhextEdit")),
            KeyBinding::new("q", Quit, Some("PhextEdit")),
        ]);

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

        cx.activate(true);
    });
}
