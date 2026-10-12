//! Colors, fonts, and the few custom widgets the UI is built from.
//! Mirrors the HTML mockup the layout was designed in.

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Layout, Margin,
    Response, RichText, Sense, Shape, Stroke, Ui, vec2,
};
pub use egui_phosphor::regular as icon;

const FILL_FAMILY: &str = "phosphor-fill";

#[derive(Clone, Copy)]
pub struct Pal {
    pub bg: Color32,
    pub panel: Color32,
    pub button: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub hover: Color32,
    pub folder: Color32,
    pub danger: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn dark() -> Pal {
    Pal {
        bg: hex(0x17191d),
        panel: hex(0x1f2226),
        button: hex(0x2a2e34),
        border: hex(0x30353c),
        text: hex(0xe4e7eb),
        muted: hex(0x8a919b),
        faint: hex(0x5d646e),
        accent: hex(0x4f8cff),
        accent_soft: Color32::from_rgba_unmultiplied(79, 140, 255, 42),
        hover: Color32::from_rgba_unmultiplied(255, 255, 255, 10),
        folder: hex(0xe0a84e),
        danger: hex(0xef5b5b),
    }
}

fn light() -> Pal {
    Pal {
        bg: hex(0xffffff),
        panel: hex(0xf4f5f7),
        button: hex(0xe9ebee),
        border: hex(0xdde1e6),
        text: hex(0x1d2025),
        muted: hex(0x5f6670),
        faint: hex(0x9aa1ab),
        accent: hex(0x2f6fec),
        accent_soft: Color32::from_rgba_unmultiplied(47, 111, 236, 34),
        hover: Color32::from_rgba_unmultiplied(0, 0, 0, 9),
        folder: hex(0xd29a35),
        danger: hex(0xd64545),
    }
}

pub fn pal(ctx: &egui::Context) -> Pal {
    if ctx.global_style().visuals.dark_mode {
        dark()
    } else {
        light()
    }
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    egui_phosphor::add_font_bytes_as_family(
        &mut fonts,
        FILL_FAMILY,
        egui_phosphor::bytes::fill::FONT,
    );
    ctx.set_fonts(fonts);
    ctx.style_mut_of(egui::Theme::Dark, |s| style(s, &dark()));
    ctx.style_mut_of(egui::Theme::Light, |s| style(s, &light()));
}

fn style(s: &mut egui::Style, p: &Pal) {
    s.spacing.item_spacing = vec2(8.0, 6.0);
    s.spacing.button_padding = vec2(10.0, 4.0);
    s.spacing.interact_size.y = 26.0;
    let v = &mut s.visuals;
    v.panel_fill = p.bg;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.hover;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.error_fg_color = p.danger;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    for (w, fill) in [
        (&mut v.widgets.inactive, p.button),
        (&mut v.widgets.hovered, p.button.gamma_multiply(1.15)),
        (&mut v.widgets.active, p.button.gamma_multiply(1.3)),
        (&mut v.widgets.open, p.button),
    ] {
        w.corner_radius = CornerRadius::same(6);
        w.weak_bg_fill = fill;
        w.bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, p.border);
        w.fg_stroke = Stroke::new(1.0, p.text);
        w.expansion = 0.0;
    }
}

/// A filled (solid) Phosphor icon.
pub fn fill(icon: &str) -> RichText {
    RichText::new(icon).family(FontFamily::Name(FILL_FAMILY.into()))
}

/// Toolbar item: icon over a small label.
pub fn tool_button(ui: &mut Ui, icon: &str, label: &str, enabled: bool, primary: bool) -> Response {
    let p = pal(ui.ctx());
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(58.0, 46.0), sense);
    if ui.is_rect_visible(rect) {
        if enabled && resp.hovered() {
            ui.painter().rect_filled(rect, 6.0, p.hover);
        }
        let alpha = if enabled { 1.0 } else { 0.35 };
        let color = if primary { p.accent } else { p.text };
        let painter = ui.painter();
        painter.text(
            rect.center_top() + vec2(0.0, 5.0),
            Align2::CENTER_TOP,
            icon,
            FontId::proportional(21.0),
            color.gamma_multiply(alpha),
        );
        painter.text(
            rect.center_bottom() - vec2(0.0, 5.0),
            Align2::CENTER_BOTTOM,
            label,
            FontId::proportional(11.0),
            p.muted.gamma_multiply(alpha),
        );
    }
    resp
}

pub fn tool_separator(ui: &mut Ui) {
    let p = pal(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(13.0, 32.0), Sense::hover());
    ui.painter()
        .vline(rect.center().x, rect.y_range(), Stroke::new(1.0, p.border));
}

/// Small frameless icon button.
pub fn icon_button(ui: &mut Ui, icon: &str, enabled: bool) -> Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(icon).size(15.0))
            .frame(false)
            .min_size(vec2(26.0, 24.0)),
    )
}

pub fn primary_button(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    let p = pal(ui.ctx());
    let button = egui::Button::new(RichText::new(text).color(Color32::WHITE))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .min_size(vec2(84.0, 28.0));
    ui.add_enabled(enabled, button)
}

pub fn danger_button(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui.ctx());
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE))
            .fill(p.danger)
            .stroke(Stroke::NONE)
            .min_size(vec2(84.0, 28.0)),
    )
}

pub fn button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(text).min_size(vec2(72.0, 28.0)))
}

/// Mutually exclusive options in one bordered strip.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) {
    let p = pal(ui.ctx());
    Frame::new()
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(6)
        .inner_margin(2)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (option, label) in options {
                    ui.selectable_value(value, *option, *label);
                }
            });
        });
}

/// Right-aligned muted label in the first column of a dialog form.
/// Painted into a fixed-width cell: layouts nested in `Grid` cells get clipped.
pub fn form_label(ui: &mut Ui, text: &str) {
    let p = pal(ui.ctx());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(13.0), p.muted);
    let (rect, _) = ui.allocate_exact_size(vec2(96.0, 26.0), Sense::hover());
    let pos = rect.right_center() - vec2(galley.size().x, galley.size().y / 2.0);
    ui.painter().galley(pos, galley, p.muted);
}

pub fn pill(ui: &mut Ui, text: &str) {
    let p = pal(ui.ctx());
    Frame::new()
        .fill(p.accent_soft)
        .corner_radius(9)
        .inner_margin(Margin::symmetric(7, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).strong().color(p.accent));
        });
}

/// Thin progress bar; `None` = indeterminate (animated).
pub fn progress_bar(ui: &mut Ui, fraction: Option<f32>, width: f32) {
    let p = pal(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, p.border);
    let span = match fraction {
        Some(f) => (0.0, f.clamp(0.0, 1.0)),
        None => {
            let t = (ui.input(|i| i.time) * 0.8).fract() as f32;
            ui.ctx().request_repaint();
            ((t * 1.3 - 0.3).max(0.0), (t * 1.3).min(1.0))
        }
    };
    let mut bar = rect;
    bar.min.x = rect.left() + rect.width() * span.0;
    bar.max.x = rect.left() + rect.width() * span.1;
    ui.painter().rect_filled(bar, 3.0, p.accent);
}

pub fn dashed_rect(ui: &Ui, rect: egui::Rect, color: Color32) {
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    ui.painter().extend(Shape::dashed_line(
        &corners,
        Stroke::new(1.5, color),
        6.0,
        5.0,
    ));
}

/// Modal card with an icon + title header. Returns whatever `body` returns.
pub fn dialog<R>(
    ctx: &egui::Context,
    icon: &str,
    title: &str,
    subtitle: &str,
    body: impl FnOnce(&mut Ui) -> R,
) -> R {
    let p = pal(ctx);
    let frame = Frame::new()
        .fill(p.panel)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(10)
        .inner_margin(Margin::same(18))
        .shadow(egui::Shadow {
            offset: [0, 12],
            blur: 40,
            spread: 0,
            color: Color32::from_black_alpha(110),
        });
    egui::Modal::new(egui::Id::new("dialog"))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(440.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).size(20.0).color(p.accent));
                ui.label(RichText::new(title).size(15.0).strong().color(p.text));
            });
            if !subtitle.is_empty() {
                ui.label(RichText::new(subtitle).size(12.0).color(p.muted));
            }
            ui.add_space(8.0);
            body(ui)
        })
        .inner
}

/// Separator + right-aligned buttons at the bottom of a dialog.
pub fn dialog_footer(ui: &mut Ui, buttons: impl FnOnce(&mut Ui)) {
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(2.0);
    ui.with_layout(Layout::right_to_left(Align::Center), buttons);
}

/// Icon for a file, by extension.
pub fn file_icon(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map_or("", |(_, e)| e)
        .to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "md" | "log" | "csv" | "rtf" | "doc" | "docx" | "odt" => icon::FILE_TEXT,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "heic" | "tif" | "tiff"
        | "ico" => icon::FILE_IMAGE,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" => icon::FILE_AUDIO,
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => icon::FILE_VIDEO,
        "pdf" => icon::FILE_PDF,
        "rs" | "c" | "h" | "cpp" | "py" | "js" | "ts" | "json" | "xml" | "html" | "css"
        | "toml" | "yaml" | "yml" | "sh" | "java" | "go" => icon::FILE_CODE,
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "bz2" | "zst" | "lz" | "tgz" => icon::FILE_ZIP,
        _ => icon::FILE,
    }
}
