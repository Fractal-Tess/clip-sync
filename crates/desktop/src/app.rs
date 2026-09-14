//! Keyboard-driven clipboard picker rendered on the CPU.
//!
//! The picker deliberately avoids a GPU context: creating one costs more than
//! the rest of startup combined, so egui output is rasterized into a softbuffer
//! surface instead.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    ops::Range,
    rc::Rc,
    time::Instant,
};

use clip_sync_ipc::protocol::HistoryUpdateAction;
use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{Key, ModifiersState, NamedKey},
    platform::wayland::WindowAttributesExtWayland as _,
    window::{Window, WindowId},
};

use crate::{
    control::Control,
    daemon::{Daemon, HistoryItem},
    theme::{
        ACCENT, BACKGROUND, CARD_BACKGROUND, CARD_SELECTED, DANGER, SELECTION, SURFACE, TEXT,
        TEXT_SELECTED,
    },
};

const WINDOW_WIDTH: f64 = 1120.0;
const WINDOW_HEIGHT: f64 = 740.0;
const HISTORY_LIMIT: u32 = 200;

/// How much larger than its nominal point size everything is drawn.
///
/// One knob rather than thirty: this is egui's points-per-pixel, so cards,
/// padding, strokes, and glyphs all grow together and the layout keeps its
/// proportions. The window grows with it so the grid keeps its column count.
const SCALE: f32 = 1.18;

/// Cards stretch to divide the row evenly; this is the narrowest one allowed
/// before the grid drops a column.
const MIN_CARD_WIDTH: f32 = 208.0;
/// Cards also stretch vertically to divide the grid evenly, so the last row
/// never leaves a band of empty background under it.
const MIN_CARD_HEIGHT: f32 = 92.0;
const CARD_PADDING: f32 = 8.0;
const GAP: f32 = 8.0;
const MARGIN: f32 = 10.0;
const HEADER_HEIGHT: f32 = 40.0;
/// Width of the control centre's navigation rail.
const NAV_WIDTH: f32 = 168.0;
const FOOTER_HEIGHT: f32 = 22.0;

/// Point size for a card's preview text.
const PREVIEW_SIZE: f32 = 11.0;
/// Point size for the small monospace badges and hints.
const META_SIZE: f32 = 9.0;

/// Thumbnails fetched per paint, so a screen of images fills in progressively
/// instead of stalling one long frame.
const PREVIEWS_PER_PASS: usize = 4;

/// Which of the two screens the window is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Picker,
    Control,
}

pub struct Picker {
    daemon: Daemon,
    started: Instant,
    timing: bool,
    items: Vec<HistoryItem>,
    filtered: Vec<usize>,
    query: String,
    /// Whether ctrl+a has selected the whole query, so the next keystroke
    /// replaces it instead of appending to it.
    query_selected: bool,
    selected: usize,
    /// Column count from the last paint, so key handling can move by a row.
    columns: usize,
    /// Positions within `filtered` that the last paint drew.
    visible: Range<usize>,
    textures: HashMap<String, egui::TextureHandle>,
    /// Entries the daemon could not produce a thumbnail for; never retried.
    undecodable: HashSet<String>,
    status: Option<String>,
    /// This node's own name, so local entries do not repeat it on every card.
    local: String,
    view: View,
    /// Built lazily: the control centre costs several daemon round trips that
    /// the picker's launch path must not pay for.
    control: Control,
    modifiers: ModifiersState,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    egui: egui::Context,
    egui_state: Option<egui_winit::State>,
    renderer: EguiSoftwareRender,
    painted: bool,
}

impl Picker {
    pub fn new(daemon: Daemon, started: Instant, control: bool) -> Self {
        let egui = egui::Context::default();
        crate::theme::install(&egui);
        Self {
            egui,
            view: if control { View::Control } else { View::Picker },
            daemon,
            started,
            timing: std::env::var_os("CLIP_SYNC_TIMING").is_some(),
            items: Vec::new(),
            filtered: Vec::new(),
            query: String::new(),
            query_selected: false,
            selected: 0,
            columns: 1,
            visible: 0..0,
            textures: HashMap::new(),
            undecodable: HashSet::new(),
            status: None,
            local: String::new(),
            control: Control::default(),
            modifiers: ModifiersState::empty(),
            window: None,
            surface: None,
            egui_state: None,
            renderer: EguiSoftwareRender::new(ColorFieldOrder::Bgra),
            painted: false,
        }
    }

    fn mark(&self, label: &str) {
        if self.timing {
            eprintln!("{label}={}ms", self.started.elapsed().as_millis());
        }
    }

    fn load_history(&mut self) {
        if self.local.is_empty() {
            self.local = self
                .daemon
                .status()
                .map(|status| status.hostname)
                .unwrap_or_default();
        }
        match self.daemon.history("", HISTORY_LIMIT) {
            Ok(items) => self.items = items,
            Err(error) => self.status = Some(format!("{error:#}")),
        }
        self.refilter();
    }

    /// Reloads history after a mutation, keeping the cursor on `content_id`.
    ///
    /// The daemon owns ordering — pinning moves an entry — so the list is
    /// refetched rather than patched in place.
    fn reload_keeping(&mut self, content_id: &str) {
        self.load_history();
        if let Some(position) = self
            .filtered
            .iter()
            .position(|&index| self.items[index].content_id == content_id)
        {
            self.selected = position;
        }
    }

    /// Recomputes the visible subset for the current query.
    ///
    /// Filtering stays in-process because the whole page is already resident;
    /// a daemon round trip per keystroke would add latency for no benefit.
    fn refilter(&mut self) {
        let needle = self.query.trim().to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                needle.is_empty()
                    || item.preview.to_lowercase().contains(&needle)
                    || item.source.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let last = self.filtered.len() - 1;
        self.selected = match delta {
            d if d < 0 => self.selected.saturating_sub(d.unsigned_abs()),
            d => (self.selected + d.unsigned_abs()).min(last),
        };
    }

    fn selected_content_id(&self) -> Option<String> {
        let &index = self.filtered.get(self.selected)?;
        Some(self.items[index].content_id.clone())
    }

    fn activate_selected(&mut self, event_loop: &ActiveEventLoop) {
        let Some(content_id) = self.selected_content_id() else {
            return;
        };
        match self.daemon.activate(&content_id) {
            Ok(_) => {
                self.mark("ACTIVATED");
                event_loop.exit();
            }
            Err(error) => {
                self.status = Some(format!("{error:#}"));
                self.request_redraw();
            }
        }
    }

    fn toggle_pin(&mut self) {
        let Some(&index) = self.filtered.get(self.selected) else {
            return;
        };
        let content_id = self.items[index].content_id.clone();
        let action = if self.items[index].pinned {
            HistoryUpdateAction::Unpin
        } else {
            HistoryUpdateAction::Pin
        };
        match self.daemon.update(&content_id, action) {
            Ok(()) => {
                self.status = None;
                self.reload_keeping(&content_id);
            }
            Err(error) => self.status = Some(format!("{error:#}")),
        }
    }

    fn delete_selected(&mut self) {
        let Some(content_id) = self.selected_content_id() else {
            return;
        };
        match self.daemon.update(&content_id, HistoryUpdateAction::Delete) {
            Ok(()) => {
                self.status = None;
                self.textures.remove(&content_id);
                let position = self.selected;
                self.load_history();
                self.selected = position.min(self.filtered.len().saturating_sub(1));
            }
            Err(error) => self.status = Some(format!("{error:#}")),
        }
    }

    fn request_redraw(&self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    /// Swaps between the picker and the control centre.
    ///
    /// Each view loads its own data on the way in, so the picker's launch path
    /// never waits on the control centre's round trips, and history picked up
    /// while the control centre was open is not missed on the way back.
    fn toggle_view(&mut self) {
        match self.view {
            View::Picker => {
                self.control.refresh(&self.daemon);
                self.view = View::Control;
            }
            View::Control => self.show_picker(),
        }
        self.request_redraw();
    }

    fn show_picker(&mut self) {
        self.load_history();
        self.view = View::Picker;
    }

    fn handle_key(&mut self, event_loop: &ActiveEventLoop, key: Key, text: Option<&str>) {
        if key == Key::Named(NamedKey::F1) {
            self.toggle_view();
            return;
        }

        // The control centre has real widgets, so egui already saw this event
        // through egui-winit; only the keys egui does not bind are handled here.
        if self.view == View::Control {
            match key {
                Key::Named(NamedKey::Escape) => self.show_picker(),
                Key::Named(NamedKey::Tab) if self.modifiers.control_key() => {
                    self.control.cycle_tab(!self.modifiers.shift_key());
                }
                _ => return,
            }
            self.request_redraw();
            return;
        }

        let columns = self.columns.max(1) as isize;
        let page = columns * 4;

        if self.modifiers.control_key() {
            match key.as_ref() {
                Key::Character("p") => self.toggle_pin(),
                Key::Character("d") => self.delete_selected(),
                Key::Character("a") => self.query_selected = !self.query.is_empty(),
                Key::Character("u") => {
                    self.query.clear();
                    self.query_selected = false;
                    self.selected = 0;
                    self.refilter();
                }
                _ => return,
            }
            self.request_redraw();
            return;
        }

        // Any key that is not ctrl+a collapses the selection, the way it would
        // in a real text field: navigating away from a selection should not
        // leave the next character silently wiping the query.
        let replacing = std::mem::take(&mut self.query_selected);

        match key {
            Key::Named(NamedKey::Escape) => {
                event_loop.exit();
                return;
            }
            Key::Named(NamedKey::Enter) => {
                self.activate_selected(event_loop);
                return;
            }
            Key::Named(NamedKey::Delete) => self.delete_selected(),
            Key::Named(NamedKey::ArrowRight) => self.move_selection(1),
            Key::Named(NamedKey::ArrowLeft) => self.move_selection(-1),
            Key::Named(NamedKey::ArrowDown) => self.move_selection(columns),
            Key::Named(NamedKey::ArrowUp) => self.move_selection(-columns),
            Key::Named(NamedKey::PageDown) => self.move_selection(page),
            Key::Named(NamedKey::PageUp) => self.move_selection(-page),
            Key::Named(NamedKey::Home) => self.selected = 0,
            Key::Named(NamedKey::End) => {
                self.selected = self.filtered.len().saturating_sub(1);
            }
            Key::Named(NamedKey::Backspace) => {
                if replacing {
                    self.query.clear();
                } else {
                    self.query.pop();
                }
                self.selected = 0;
                self.refilter();
            }
            _ => {
                let typed = text.unwrap_or_default();
                if typed.is_empty() || typed.chars().any(char::is_control) {
                    return;
                }
                if replacing {
                    self.query.clear();
                }
                self.query.push_str(typed);
                self.selected = 0;
                self.refilter();
            }
        }
        self.request_redraw();
    }

    /// Fetches a bounded number of thumbnails for cards currently on screen.
    ///
    /// Returns whether more work remains, so the caller can schedule another
    /// paint and let the grid fill in rather than block on a full screen of
    /// image decodes.
    fn hydrate_previews(&mut self) -> bool {
        let pending: Vec<String> = self
            .filtered
            .get(self.visible.clone())
            .unwrap_or_default()
            .iter()
            .map(|&index| &self.items[index])
            .filter(|item| item.is_image)
            .map(|item| item.content_id.clone())
            .filter(|id| !self.textures.contains_key(id) && !self.undecodable.contains(id))
            .collect();

        for content_id in pending.iter().take(PREVIEWS_PER_PASS) {
            match self.daemon.image_preview(content_id) {
                Ok(preview) => {
                    let size = [preview.width as usize, preview.height as usize];
                    let image = egui::ColorImage::from_rgba_unmultiplied(size, &preview.rgba);
                    let handle =
                        self.egui
                            .load_texture(content_id, image, egui::TextureOptions::LINEAR);
                    self.textures.insert(content_id.clone(), handle);
                }
                Err(_) => {
                    self.undecodable.insert(content_id.clone());
                }
            }
        }

        !pending.is_empty()
    }

    fn draw(&mut self, width: u32, height: u32) -> egui::FullOutput {
        let mut input = match (self.egui_state.as_mut(), self.window.as_ref()) {
            (Some(state), Some(window)) => state.take_egui_input(window),
            _ => egui::RawInput::default(),
        };
        // egui lays out in points, the surface is addressed in pixels, and
        // `SCALE` is the ratio between them.
        let points = egui::vec2(width as f32, height as f32) / SCALE;
        input.screen_rect = Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), points));

        match self.view {
            View::Picker => self.draw_picker(input, points),
            View::Control => self.draw_control(input),
        }
    }

    fn draw_control(&mut self, input: egui::RawInput) -> egui::FullOutput {
        let mut actions = Vec::new();
        let control = &mut self.control;
        let output = self.egui.run_ui(input, |root| {
            egui::Panel::left("control_nav")
                .exact_size(NAV_WIDTH)
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(SURFACE)
                        .inner_margin(egui::Margin::symmetric(12, 14)),
                )
                .show_inside(root, |ui| control.nav(ui, &mut actions));
            egui::Panel::bottom("control_footer")
                .frame(
                    egui::Frame::new()
                        .fill(BACKGROUND)
                        .inner_margin(egui::Margin::symmetric(18, 8)),
                )
                .show_inside(root, |ui| {
                    ui.label(
                        egui::RichText::new("^tab switch tab    f1 picker    esc back")
                            .monospace()
                            .size(9.0)
                            .weak(),
                    );
                });
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(BACKGROUND)
                        .inner_margin(egui::Margin::symmetric(18, 14)),
                )
                .show_inside(root, |ui| control.body(ui, &mut actions));
        });
        for action in actions {
            self.control.apply(&self.daemon, action);
        }
        output
    }

    #[allow(clippy::too_many_lines)]
    fn draw_picker(&mut self, input: egui::RawInput, points: egui::Vec2) -> egui::FullOutput {
        let grid_width = points.x - 2.0 * MARGIN;
        let columns = (((grid_width + GAP) / (MIN_CARD_WIDTH + GAP)).floor() as usize).max(1);
        let card_width = (grid_width - GAP * (columns - 1) as f32) / columns as f32;
        let grid_height = points.y - HEADER_HEIGHT - FOOTER_HEIGHT - 2.0 * MARGIN;
        let visible_rows =
            (((grid_height + GAP) / (MIN_CARD_HEIGHT + GAP)).floor() as usize).max(1);
        let card_height =
            (grid_height - GAP * (visible_rows - 1) as f32) / visible_rows as f32;

        let first_row = (self.selected / columns).saturating_sub(visible_rows.saturating_sub(1));
        let start = first_row * columns;
        let end = (start + visible_rows * columns).min(self.filtered.len());
        let visible = start..end;

        let rows: Vec<Vec<(usize, bool)>> = self
            .filtered
            .get(visible.clone())
            .unwrap_or_default()
            .chunks(columns)
            .enumerate()
            .map(|(row, chunk)| {
                chunk
                    .iter()
                    .enumerate()
                    .map(|(column, &index)| {
                        (index, start + row * columns + column == self.selected)
                    })
                    .collect()
            })
            .collect();

        let query = self.query.clone();
        let query_selected = self.query_selected;
        let status = self.status.clone();
        let now = unix_millis();
        let total = self.items.len();
        let shown = self.filtered.len();
        let position = if shown == 0 { 0 } else { self.selected + 1 };
        let items = &self.items;
        let textures = &self.textures;
        let local = self.local.as_str();

        let output = self.egui.run_ui(input, |root| {
            let frame = egui::Frame::new().fill(BACKGROUND).inner_margin(MARGIN);
            egui::CentralPanel::default()
                .frame(frame)
                .show_inside(root, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);

                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("›").monospace().size(16.0).color(ACCENT));
                        // The search is never unfocused, so the caret is always
                        // drawn: it is the only thing telling the user that
                        // typing goes here rather than into the grid. It does
                        // not blink, because animating it would mean asking for
                        // a repaint several times a second forever.
                        ui.spacing_mut().item_spacing.x = 3.0;
                        if !query.is_empty() {
                            // Laid out by hand rather than with `ui.label` so
                            // the highlight can go down before the glyphs:
                            // ctrl+a has no caret to move, so this block is the
                            // only feedback that the query is selected.
                            let galley = ui.painter().layout_no_wrap(
                                query.clone(),
                                egui::FontId::proportional(14.0),
                                TEXT_SELECTED,
                            );
                            let (rect, _) = ui
                                .allocate_exact_size(galley.size(), egui::Sense::hover());
                            if query_selected {
                                ui.painter().rect_filled(
                                    rect.expand2(egui::vec2(2.0, 2.0)),
                                    2.0,
                                    SELECTION,
                                );
                            }
                            ui.painter().galley(rect.min, galley, TEXT_SELECTED);
                        }
                        let (caret, _) =
                            ui.allocate_exact_size(egui::vec2(2.0, 17.0), egui::Sense::hover());
                        ui.painter().rect_filled(caret, 1.0, ACCENT);
                        if query.is_empty() {
                            ui.label(
                                egui::RichText::new("type to filter…")
                                    .size(14.0)
                                    .weak()
                                    .italics(),
                            );
                        }
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let counter = if shown == total {
                                    format!("{position}/{total}")
                                } else {
                                    format!("{position}/{shown} of {total}")
                                };
                                ui.label(
                                    egui::RichText::new(counter)
                                        .monospace()
                                        .size(META_SIZE + 1.0)
                                        .weak(),
                                );
                            },
                        );
                    });

                    if let Some(status) = status.as_ref() {
                        ui.colored_label(DANGER, status);
                    }

                    if rows.is_empty() {
                        ui.add_space(GAP);
                        ui.label(egui::RichText::new("No matching entries").weak());
                    }

                    for row in &rows {
                        ui.horizontal(|ui| {
                            for (index, selected) in row {
                                card(
                                    ui,
                                    &items[*index],
                                    *selected,
                                    textures,
                                    egui::vec2(card_width, card_height),
                                    local,
                                    now,
                                );
                            }
                        });
                    }

                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        ui.label(
                            egui::RichText::new(
                                "←↑↓→ move    enter copy    ^a select all    ^p pin    ^d delete    f1 control    esc close",
                            )
                            .monospace()
                            .size(META_SIZE)
                            .weak(),
                        );
                    });
                });
        });

        self.columns = columns;
        self.visible = visible;
        output
    }

    fn paint(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        let (width, height) = (size.width.max(1), size.height.max(1));

        let mut output = self.draw(width, height);
        if let Some(state) = self.egui_state.as_mut() {
            state.handle_platform_output(&window, std::mem::take(&mut output.platform_output));
        }
        // egui asks for another frame while something is animating and stops
        // asking once it settles, which is what keeps this from spinning.
        let animating = output
            .viewport_output
            .values()
            .any(|viewport| viewport.repaint_delay.is_zero());
        let primitives = self.egui.tessellate(output.shapes, SCALE);

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let (Some(nz_width), Some(nz_height)) = (NonZeroU32::new(width), NonZeroU32::new(height))
        else {
            return;
        };
        if surface.resize(nz_width, nz_height).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        buffer.fill(0xff_0c_11_14);
        {
            let pixels: &mut [[u8; 4]] = bytemuck::cast_slice_mut(&mut buffer);
            let mut target = BufferMutRef::new(pixels, width as usize, height as usize);
            self.renderer
                .render(&mut target, &primitives, &output.textures_delta, SCALE);
        }
        let _ = buffer.present();

        if !self.painted {
            self.painted = true;
            self.mark("FIRST_PIXEL");
        }

        // Thumbnails are fetched only after the grid is on screen, so image
        // decoding never delays the first frame.
        if animating || (self.view == View::Picker && self.hydrate_previews()) {
            self.request_redraw();
        }
    }
}

impl ApplicationHandler for Picker {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Without an app_id the compositor sees an empty class, so Hyprland
        // window rules cannot target the picker and it tiles like an ordinary
        // application instead of floating over the workspace.
        let attributes = Window::default_attributes()
            .with_title("ClipSync")
            .with_name("clip-sync", "clip-sync")
            .with_inner_size(winit::dpi::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
        let Ok(window) = event_loop.create_window(attributes) else {
            event_loop.exit();
            return;
        };
        let window = Rc::new(window);
        self.mark("WINDOW");

        match softbuffer::Context::new(window.clone())
            .and_then(|context| softbuffer::Surface::new(&context, window.clone()))
        {
            Ok(surface) => self.surface = Some(surface),
            Err(error) => {
                eprintln!("clip-sync: failed to create a drawing surface: {error}");
                event_loop.exit();
                return;
            }
        }
        self.egui_state = Some(egui_winit::State::new(
            self.egui.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(SCALE),
            None,
            Some(2048),
        ));
        self.mark("EGUI_STATE");
        self.window = Some(window);

        match self.view {
            View::Picker => self.load_history(),
            View::Control => self.control.refresh(&self.daemon),
        }
        self.mark("HISTORY");
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // egui sees every event so the control centre's widgets work; the
        // picker's own key handling is gated on the active view instead.
        // RedrawRequested is the exception: egui-winit answers it with
        // `repaint: true`, which would schedule the next frame from inside the
        // current one and peg a core.
        if !matches!(event, WindowEvent::RedrawRequested)
            && let (Some(state), Some(window)) = (self.egui_state.as_mut(), self.window.as_ref())
            && state.on_window_event(window, &event).repaint
        {
            window.request_redraw();
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let text = event.text.as_deref();
                self.handle_key(event_loop, event.logical_key.clone(), text);
            }
            _ => {}
        }
    }
}

/// Draws one history card at the size the grid allotted it.
fn card(
    ui: &mut egui::Ui,
    item: &HistoryItem,
    selected: bool,
    textures: &HashMap<String, egui::TextureHandle>,
    size: egui::Vec2,
    local: &str,
    now: u64,
) {
    let (background, foreground) = if selected {
        (CARD_SELECTED, TEXT_SELECTED)
    } else {
        (CARD_BACKGROUND, TEXT)
    };
    let stroke = if selected {
        egui::Stroke::new(1.0_f32, ACCENT)
    } else {
        egui::Stroke::NONE
    };

    ui.allocate_ui(size, |ui| {
        egui::Frame::new()
            .fill(background)
            .stroke(stroke)
            .corner_radius(6)
            .inner_margin(CARD_PADDING)
            .show(ui, |ui| {
                let inner = size - egui::Vec2::splat(2.0 * CARD_PADDING);
                ui.set_min_size(inner);
                ui.set_max_size(inner);
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.vertical(|ui| {
                    // Reserve the metadata row plus the spacing above it, so a
                    // long preview is clipped clear of the row rather than
                    // running its last line into the size and age.
                    let body_height = inner.y - META_SIZE - 9.0;
                    ui.allocate_ui(egui::vec2(inner.x, body_height), |ui| {
                        ui.set_clip_rect(ui.max_rect());
                        match textures.get(&item.content_id) {
                            Some(handle) => {
                                // Thumbnails keep their aspect ratio and sit in
                                // the middle of the card, so a very wide or very
                                // tall capture still reads as a picture rather
                                // than a stripe pinned to one edge.
                                let (area, _) = ui.allocate_exact_size(
                                    egui::vec2(inner.x, body_height),
                                    egui::Sense::hover(),
                                );
                                let scaled = fit(handle.size_vec2(), area.size());
                                let texture =
                                    egui::load::SizedTexture::new(handle.id(), scaled);
                                egui::Image::new(texture).corner_radius(4).paint_at(
                                    ui,
                                    egui::Rect::from_center_size(area.center(), scaled),
                                );
                            }
                            None => {
                                ui.label(
                                    egui::RichText::new(clamp_preview(&item.preview))
                                        .color(foreground)
                                        .size(PREVIEW_SIZE),
                                );
                            }
                        }
                    });

                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        ui.horizontal(|ui| {
                            if item.pinned {
                                ui.label(
                                    egui::RichText::new("pin")
                                        .monospace()
                                        .size(META_SIZE)
                                        .color(ACCENT),
                                );
                            }
                            if item.is_image {
                                ui.label(
                                    egui::RichText::new("img")
                                        .monospace()
                                        .size(META_SIZE)
                                        .weak(),
                                );
                            }
                            // Entries from this machine are the common case, so
                            // only remote origins earn a label.
                            if item.source != local && !item.source.is_empty() {
                                ui.label(
                                    egui::RichText::new(&item.source)
                                        .monospace()
                                        .size(META_SIZE)
                                        .weak(),
                                );
                            }
                            // Age and size sit on the right so they line up
                            // down the column and stay readable as a pair,
                            // rather than shifting with the badges beside them.
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{}  ·  {}",
                                            human_size(item.size_bytes),
                                            human_age(now, item.created_millis),
                                        ))
                                        .monospace()
                                        .size(META_SIZE)
                                        .weak(),
                                    );
                                },
                            );
                        });
                    });
                });
            });
    });
}

/// Wall-clock milliseconds since the Unix epoch.
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis().min(u128::from(u64::MAX)) as u64)
}

/// Renders a byte count in the largest unit that keeps it under four digits.
///
/// Cards have room for a handful of characters, so precision is traded for
/// width: one decimal below 10 of a unit, none above it.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Renders how long ago `created` was, as a single coarse unit.
///
/// Clipboard history is browsed by recency, so "3h" answers the question the
/// user actually has; an absolute timestamp would cost twice the width to say
/// less. Entries stamped in the future — clock skew across the mesh — read as
/// "now" rather than wrapping into a negative age.
fn human_age(now: u64, created: u64) -> String {
    let seconds = now.saturating_sub(created) / 1000;
    match seconds {
        0..=9 => "now".to_owned(),
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 604_800 => format!("{}d", s / 86_400),
        s if s < 2_592_000 => format!("{}w", s / 604_800),
        s if s < 31_536_000 => format!("{}mo", s / 2_592_000),
        s => format!("{}y", s / 31_536_000),
    }
}

/// Scales `source` down to fit inside `bounds` without distorting it.
fn fit(source: egui::Vec2, bounds: egui::Vec2) -> egui::Vec2 {
    if source.x <= 0.0 || source.y <= 0.0 {
        return bounds;
    }
    let scale = (bounds.x / source.x).min(bounds.y / source.y).min(1.0);
    source * scale
}

/// Number of characters a card can plausibly show before egui clips it.
const PREVIEW_CHARS: usize = 180;

/// Trims a preview to roughly what one card can show.
///
/// Long clipboard entries are common and laying out thousands of glyphs that
/// are then clipped away is pure cost, so the text is cut before egui sees it.
/// Wrapping is left to egui, which knows the real glyph widths.
fn clamp_preview(preview: &str) -> String {
    let mut out = String::new();
    for (index, line) in preview.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() && out.is_empty() {
            continue;
        }
        if index > 0 && !out.is_empty() {
            out.push(' ');
        }
        out.push_str(trimmed);
        if out.chars().count() >= PREVIEW_CHARS {
            break;
        }
    }
    if out.chars().count() > PREVIEW_CHARS {
        out = out.chars().take(PREVIEW_CHARS).collect();
        out.push('…');
    }
    out
}
