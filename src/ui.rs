use eframe::egui::{
    self, Color32, CornerRadius, FontId, Frame, Key, Margin, RichText, Stroke, Vec2,
};

const BACKGROUND: Color32 = Color32::from_rgb(12, 17, 20);
const SURFACE: Color32 = Color32::from_rgb(20, 28, 32);
const BORDER: Color32 = Color32::from_rgb(44, 63, 70);
const CYAN: Color32 = Color32::from_rgb(35, 200, 226);
const MUTED: Color32 = Color32::from_rgb(137, 154, 160);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Switcher,
    Control,
}

/// Starts the optional native egui process.
///
/// # Errors
///
/// Returns an error when the native event loop or graphics context cannot start.
pub fn run(mode: UiMode) -> Result<(), String> {
    let (title, app_id, size, decorations) = match mode {
        UiMode::Switcher => (
            "clip-sync switcher",
            "clip-sync-switcher",
            Vec2::new(720.0, 420.0),
            false,
        ),
        UiMode::Control => (
            "clip-sync control center",
            "clip-sync-control",
            Vec2::new(1040.0, 700.0),
            true,
        ),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_app_id(app_id)
            .with_inner_size(size)
            .with_min_inner_size(Vec2::new(480.0, 300.0))
            .with_decorations(decorations),
        ..Default::default()
    };

    eframe::run_native(
        title,
        options,
        Box::new(move |context| {
            configure_style(&context.egui_ctx);
            Ok(Box::new(ClipSyncApp::new(mode)))
        }),
    )
    .map_err(|error| error.to_string())
}

fn configure_style(context: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = BACKGROUND;
    visuals.extreme_bg_color = SURFACE;
    visuals.faint_bg_color = SURFACE;
    visuals.selection.bg_fill = CYAN;
    visuals.selection.stroke = Stroke::new(1.0, Color32::BLACK);
    visuals.widgets.inactive.bg_fill = SURFACE;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, CYAN);
    visuals.widgets.active.bg_fill = CYAN;
    visuals.window_corner_radius = CornerRadius::same(10);
    context.set_visuals(visuals);

    let mut style = (*context.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(12.0, 8.0);
    context.set_style_of(egui::Theme::Dark, style);
}

struct ClipSyncApp {
    mode: UiMode,
    search: String,
    selected_tab: ControlTab,
    first_frame: bool,
}

impl ClipSyncApp {
    fn new(mode: UiMode) -> Self {
        Self {
            mode,
            search: String::new(),
            selected_tab: ControlTab::History,
            first_frame: true,
        }
    }

    fn switcher(&mut self, ui: &mut egui::Ui) {
        if ui.input(|input| input.key_pressed(Key::Escape)) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        Frame::new()
            .fill(BACKGROUND)
            .inner_margin(Margin::same(22))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("CLIP").strong().color(CYAN).size(13.0));
                    ui.label(
                        RichText::new("SYNC")
                            .strong()
                            .color(Color32::WHITE)
                            .size(13.0),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new("history switcher").color(MUTED).size(12.0));
                });
                ui.add_space(8.0);

                let search = ui.add_sized(
                    [ui.available_width(), 42.0],
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search history · device:kiwi type:text")
                        .font(FontId::proportional(16.0)),
                );
                if self.first_frame {
                    search.request_focus();
                    self.first_frame = false;
                }

                ui.add_space(6.0);
                placeholder_panel(
                    ui,
                    "History is not connected yet",
                    "The egui shell is ready. Encrypted local history lands in Milestone 2.",
                );
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new("↑↓ navigate    Enter copy    Esc close")
                            .monospace()
                            .color(MUTED)
                            .size(11.0),
                    );
                });
            });
    }

    fn control(&mut self, ui: &mut egui::Ui) {
        Frame::new()
            .fill(BACKGROUND)
            .inner_margin(Margin::same(20))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("clip-sync").color(Color32::WHITE));
                    ui.label(
                        RichText::new("CONTROL CENTER")
                            .color(CYAN)
                            .monospace()
                            .size(11.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("daemon foundation").color(MUTED).size(12.0));
                        ui.colored_label(CYAN, "●");
                    });
                });
                ui.separator();

                ui.horizontal(|ui| {
                    for tab in ControlTab::ALL {
                        let selected = self.selected_tab == tab;
                        if ui.selectable_label(selected, tab.label()).clicked() {
                            self.selected_tab = tab;
                        }
                    }
                });
                ui.add_space(12.0);

                let (title, detail) = self.selected_tab.placeholder();
                placeholder_panel(ui, title, detail);
            });
    }
}

impl eframe::App for ClipSyncApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.mode {
            UiMode::Switcher => self.switcher(ui),
            UiMode::Control => self.control(ui),
        }
    }
}

fn placeholder_panel(ui: &mut egui::Ui, title: &str, detail: &str) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_min_height(180.0);
            ui.vertical_centered(|ui| {
                ui.add_space(42.0);
                ui.label(
                    RichText::new(title)
                        .color(Color32::WHITE)
                        .size(16.0)
                        .strong(),
                );
                ui.label(RichText::new(detail).color(MUTED).size(13.0));
            });
        });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlTab {
    History,
    Share,
    Transfers,
    Peers,
    Settings,
    Diagnostics,
}

impl ControlTab {
    const ALL: [Self; 6] = [
        Self::History,
        Self::Share,
        Self::Transfers,
        Self::Peers,
        Self::Settings,
        Self::Diagnostics,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::History => "History",
            Self::Share => "Share",
            Self::Transfers => "Transfers",
            Self::Peers => "Peers",
            Self::Settings => "Settings",
            Self::Diagnostics => "Diagnostics",
        }
    }

    const fn placeholder(self) -> (&'static str, &'static str) {
        match self {
            Self::History => (
                "Merged history",
                "Encrypted history storage lands in Milestone 2.",
            ),
            Self::Share => (
                "Share current clipboard",
                "Large-item inspection and sharing land in Milestone 6.",
            ),
            Self::Transfers => (
                "No active transfers",
                "Resumable chunk transfers land in Milestone 6.",
            ),
            Self::Peers => (
                "Peer discovery ready",
                "Authenticated peer sessions land in Milestone 3.",
            ),
            Self::Settings => (
                "Mesh settings",
                "Validated TOML defaults are active; live editing is not connected yet.",
            ),
            Self::Diagnostics => (
                "Diagnostics",
                "Use `clip-sync doctor` for current foundation checks.",
            ),
        }
    }
}
