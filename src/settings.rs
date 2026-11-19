//! Janela "Preferências do Dock": tamanho, ampliação, posição na tela e comportamentos.
//! As mudanças são aplicadas na hora; o arquivo é gravado ao soltar o controle.

use tiny_skia::{Color, Pixmap, Transform};
use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

use crate::anim::Spring;
use crate::config::{Edge, HideMode, Settings, ICON_MAX, ICON_MIN, MAG_MAX, MAG_MIN, TEXT_MAX, TEXT_MIN};
use crate::draw;
use crate::{x, Action, Ctx, R};

const W: f32 = 640.0;
const H: f32 = 680.0;
const LABEL_X: f32 = 28.0;
const CTRL_X: f32 = 270.0;
const CTRL_END: f32 = W - 28.0;
const ACCENT: (u8, u8, u8) = (10, 132, 255);

const ROW_SIZE: f32 = 46.0;
const ROW_MAG: f32 = 114.0;
const ROW_EDGE: f32 = 182.0;
const ROW_TEXT: f32 = 250.0;
const ROW_THEME: f32 = 316.0;
const THEME_BTN: f32 = 34.0;
const DIVIDER: f32 = 366.0;
const ROW_HIDE: f32 = 408.0;
const ROW_TOGGLES: [f32; 5] = [454.0, 500.0, 546.0, 592.0, 638.0];
const N_SWITCH: usize = 6;
const SWITCH_W: f32 = 40.0;
const SWITCH_H: f32 = 24.0;
const MAG_SLIDER_X: f32 = CTRL_X + SWITCH_W + 18.0;

#[derive(Clone, Copy, PartialEq)]
enum Slider {
    Size,
    Mag,
    Text,
}

pub struct SettingsWin {
    pub win: Window,
    out: x::Surface,
    pix: Pixmap,
    pub visible: bool,
    /// Ampliação, bounce, indicadores, prévias, destaque ao arrastar, recentes.
    switches: [Spring; N_SWITCH],
    segment: Spring,
    hide_segment: Spring,
    active: Option<Slider>,
    pub needs_frame: bool,
}

fn switch_values(s: &Settings) -> [bool; N_SWITCH] {
    [s.magnification, s.bounce, s.indicators, s.previews, s.drag_highlight, s.recents]
}

fn hide_index(h: HideMode) -> f32 {
    match h {
        HideMode::Never => 0.0,
        HideMode::Always => 1.0,
        HideMode::Smart => 2.0,
    }
}

fn edge_index(e: Edge) -> f32 {
    match e {
        Edge::Left => 0.0,
        Edge::Bottom => 1.0,
        Edge::Right => 2.0,
    }
}

fn in_rect(x: f32, y: f32, r: (f32, f32, f32, f32)) -> bool {
    x >= r.0 && x <= r.0 + r.2 && y >= r.1 && y <= r.1 + r.3
}

fn switch_rect(i: usize) -> (f32, f32, f32, f32) {
    if i == 0 {
        (CTRL_X, ROW_MAG - SWITCH_H / 2.0, SWITCH_W, SWITCH_H)
    } else {
        (CTRL_END - SWITCH_W, ROW_TOGGLES[i - 1] - SWITCH_H / 2.0, SWITCH_W, SWITCH_H)
    }
}

fn slider_span(s: Slider) -> (f32, f32, f32) {
    match s {
        Slider::Size => (CTRL_X, CTRL_END, ROW_SIZE),
        Slider::Mag => (MAG_SLIDER_X, CTRL_END, ROW_MAG),
        Slider::Text => (CTRL_X, CTRL_END, ROW_TEXT),
    }
}

fn segment_rect(cy: f32) -> (f32, f32, f32, f32) {
    (CTRL_X, cy - 15.0, CTRL_END - CTRL_X, 30.0)
}

impl SettingsWin {
    pub fn new(ctx: &mut Ctx) -> R<SettingsWin> {
        let (mx, my, mw, mh) = ctx.mon;
        let geom = (mx + (mw - W as i32) / 2, my + (mh - H as i32) / 2, W as i32, H as i32);
        let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
        let conn = ctx.conn.clone();
        let (win, out) = x::create_window(&conn, ctx.root, ctx.visual, ctx.colormap, geom, false, events)?;
        let a = &ctx.atoms;
        let title = "Preferências do Dock";
        conn.change_property8(PropMode::REPLACE, win, a._NET_WM_NAME, a.UTF8_STRING, title.as_bytes())?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"Preferencias do Dock")?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"rdock-settings\0Rdock\0")?;
        conn.change_property32(PropMode::REPLACE, win, a.WM_PROTOCOLS, AtomEnum::ATOM, &[a.WM_DELETE_WINDOW])?;
        conn.change_property32(PropMode::REPLACE, win, a._NET_WM_WINDOW_TYPE, AtomEnum::ATOM, &[a._NET_WM_WINDOW_TYPE_DIALOG])?;
        let mut hints = WmSizeHints::new();
        hints.position = Some((WmSizeHintsSpecification::UserSpecified, geom.0, geom.1));
        hints.min_size = Some((geom.2, geom.3));
        hints.max_size = Some((geom.2, geom.3));
        hints.set_normal_hints(&*conn, win)?;
        ctx.own.push(win);
        let s = ctx.settings;
        let sw = switch_values(&s).map(|on| Spring::new(if on { 1.0 } else { 0.0 }, 500.0, 0.75));
        Ok(SettingsWin {
            win,
            out,
            pix: Pixmap::new(W as u32, H as u32).ok_or("pixmap")?,
            visible: false,
            switches: sw,
            segment: Spring::new(edge_index(s.edge), 400.0, 0.8),
            hide_segment: Spring::new(hide_index(s.hide), 400.0, 0.8),
            active: None,
            needs_frame: false,
        })
    }

    pub fn open(&mut self, ctx: &mut Ctx) -> R<()> {
        if !self.visible {
            ctx.conn.map_window(self.win)?;
            self.visible = true;
        }
        x::activate(&ctx.conn, &ctx.atoms, ctx.root, self.win, ctx.time);
        self.needs_frame = true;
        Ok(())
    }

    pub fn close(&mut self, ctx: &mut Ctx) {
        if self.visible {
            let _ = ctx.conn.unmap_window(self.win);
            self.visible = false;
            self.active = None;
        }
    }

    pub fn damage(&mut self) {
        if self.visible {
            self.needs_frame = true;
        }
    }

    // ---------------------------------------------------------------- entrada

    fn slider_value(s: Slider, x: f32) -> f32 {
        let (x0, x1, _) = slider_span(s);
        let t = ((x - x0) / (x1 - x0)).clamp(0.0, 1.0);
        match s {
            Slider::Size => (ICON_MIN + t * (ICON_MAX - ICON_MIN)).round(),
            Slider::Mag => (MAG_MIN + t * (MAG_MAX - MAG_MIN)).round(),
            // passos de 5%
            Slider::Text => ((TEXT_MIN + t * (TEXT_MAX - TEXT_MIN)) * 20.0).round() / 20.0,
        }
    }

    fn with_slider(mut s: Settings, which: Slider, x: f32) -> Settings {
        match which {
            Slider::Size => s.icon_size = Self::slider_value(which, x),
            Slider::Mag => s.max_size = Self::slider_value(which, x),
            Slider::Text => s.text_scale = Self::slider_value(which, x),
        }
        s
    }

    pub fn on_press(&mut self, ctx: &Ctx, button: u8, x: f32, y: f32) -> Option<Action> {
        if button != 1 {
            return None;
        }
        self.needs_frame = true;
        let mut s = ctx.settings;
        for which in [Slider::Size, Slider::Mag, Slider::Text] {
            let (x0, x1, cy) = slider_span(which);
            if which == Slider::Mag && !s.magnification {
                continue;
            }
            if x >= x0 - 12.0 && x <= x1 + 12.0 && (y - cy).abs() <= 14.0 {
                self.active = Some(which);
                return Some(Action::ApplySettings(Self::with_slider(s, which, x), false));
            }
        }
        for i in 0..N_SWITCH {
            let (rx, ry, rw, rh) = switch_rect(i);
            // Nas linhas de baixo, clicar no texto também alterna (como num checkbox).
            let hit = if i == 0 { in_rect(x, y, (rx - 4.0, ry - 4.0, rw + 8.0, rh + 8.0)) } else { (y - (ry + rh / 2.0)).abs() <= 18.0 };
            if hit {
                match i {
                    0 => s.magnification = !s.magnification,
                    1 => s.bounce = !s.bounce,
                    2 => s.indicators = !s.indicators,
                    3 => s.previews = !s.previews,
                    4 => s.drag_highlight = !s.drag_highlight,
                    _ => s.recents = !s.recents,
                }
                return Some(Action::ApplySettings(s, true));
            }
        }
        // Tema: setas para trocar, link para abrir a pasta de temas.
        let (tx, ty, tw, th) = segment_rect(ROW_THEME);
        if (y - ROW_THEME).abs() <= th / 2.0 && ctx.themes.len() > 1 {
            let n = ctx.themes.len();
            let cur = ctx.themes.iter().position(|t| t.name == ctx.theme).unwrap_or(0);
            if x >= tx && x <= tx + THEME_BTN {
                return Some(Action::SetTheme((cur + n - 1) % n));
            }
            if x >= tx + tw - THEME_BTN && x <= tx + tw {
                return Some(Action::SetTheme((cur + 1) % n));
            }
        }
        let link_w = ctx.text.width("Abrir pasta de temas", Self::fit(ctx, 13.0));
        if in_rect(x, y, (tx, ty + th + 2.0, link_w + 4.0, 20.0)) {
            return Some(Action::OpenThemeFolder);
        }
        let pick = |seg: (f32, f32, f32, f32)| (((x - seg.0) / seg.2) * 3.0).floor().clamp(0.0, 2.0) as usize;
        let seg = segment_rect(ROW_EDGE);
        if in_rect(x, y, seg) {
            s.edge = [Edge::Left, Edge::Bottom, Edge::Right][pick(seg)];
            return Some(Action::ApplySettings(s, true));
        }
        let seg = segment_rect(ROW_HIDE);
        if in_rect(x, y, seg) {
            s.hide = [HideMode::Never, HideMode::Always, HideMode::Smart][pick(seg)];
            return Some(Action::ApplySettings(s, true));
        }
        None
    }

    pub fn on_motion(&mut self, ctx: &Ctx, x: f32, _y: f32) -> Option<Action> {
        let which = self.active?;
        self.needs_frame = true;
        let s = Self::with_slider(ctx.settings, which, x);
        (s != ctx.settings).then_some(Action::ApplySettings(s, false))
    }

    pub fn on_release(&mut self, ctx: &Ctx) -> Option<Action> {
        self.active.take().map(|_| Action::ApplySettings(ctx.settings, true))
    }

    // ---------------------------------------------------------------- animação e desenho

    pub fn tick(&mut self, ctx: &Ctx, dt: f32) -> bool {
        let s = ctx.settings;
        let mut anim = false;
        for (sp, on) in self.switches.iter_mut().zip(switch_values(&s)) {
            sp.target = if on { 1.0 } else { 0.0 };
            anim |= sp.step(dt);
        }
        self.segment.target = edge_index(s.edge);
        anim |= self.segment.step(dt);
        self.hide_segment.target = hide_index(s.hide);
        anim |= self.hide_segment.step(dt);
        anim
    }

    /// Nesta janela o texto cresce até 140%, para os rótulos não invadirem os controles.
    fn fit(ctx: &Ctx, size: f32) -> f32 {
        size * (1.4 / ctx.text.scale).min(1.0)
    }

    fn label(&mut self, ctx: &mut Ctx, text: &str, x: f32, cy: f32, size: f32, alpha: f32) {
        let size = Self::fit(ctx, size);
        let tp = ctx.text.get(text, size, 0xffffff);
        let y = (cy - tp.height() as f32 / 2.0).round();
        draw::blit(&mut self.pix, tp, Transform::from_translate(x.round(), y), alpha);
    }

    fn label_right(&mut self, ctx: &mut Ctx, text: &str, right: f32, cy: f32, size: f32, alpha: f32) {
        let w = ctx.text.width(text, Self::fit(ctx, size)) + 4.0;
        self.label(ctx, text, right - w, cy, size, alpha);
    }

    fn slider(&mut self, which: Slider, value: f32, range: (f32, f32), enabled: f32) {
        let (x0, x1, cy) = slider_span(which);
        let t = ((value - range.0) / (range.1 - range.0)).clamp(0.0, 1.0);
        let kx = x0 + t * (x1 - x0);
        let id = Transform::identity();
        let a = 0.35 + 0.65 * enabled;
        draw::fill(&mut self.pix, &draw::rrect(x0, cy - 2.0, x1 - x0, 4.0, 2.0), &draw::color(255, 255, 255, 0.16), id);
        let (r, g, b) = ACCENT;
        draw::fill(&mut self.pix, &draw::rrect(x0, cy - 2.0, kx - x0, 4.0, 2.0), &draw::color(r, g, b, a), id);
        let pressed = self.active == Some(which);
        let kr = if pressed { 10.5 } else { 9.5 };
        draw::shadow_n(&mut self.pix, kx - kr, cy - kr, 2.0 * kr, 2.0 * kr, kr, 0.9, id, 4);
        draw::circle(&mut self.pix, kx, cy, kr, &draw::color(255, 255, 255, a));
    }

    /// Controle segmentado de 3 opções com seleção deslizante (`pos` = índice animado).
    fn segmented(&mut self, ctx: &mut Ctx, cy: f32, labels: [&str; 3], pos: f32) {
        let id = Transform::identity();
        let (sx, sy, sw, sh) = segment_rect(cy);
        draw::fill(&mut self.pix, &draw::rrect(sx, sy, sw, sh, 8.0), &draw::color(255, 255, 255, 0.09), id);
        let seg_w = sw / 3.0;
        let px = sx + 2.0 + pos * seg_w;
        draw::shadow_n(&mut self.pix, px, sy + 2.0, seg_w - 4.0, sh - 4.0, 6.0, 0.6, id, 3);
        draw::fill(&mut self.pix, &draw::rrect(px, sy + 2.0, seg_w - 4.0, sh - 4.0, 6.0), &draw::color(100, 100, 106, 1.0), id);
        for (k, l) in labels.iter().enumerate() {
            let w = ctx.text.width(l, Self::fit(ctx, 15.0));
            let near = (1.0 - (pos - k as f32).abs()).max(0.0);
            self.label(ctx, l, sx + k as f32 * seg_w + (seg_w - w) / 2.0 - 2.0, cy, 15.0, 0.7 + 0.3 * near);
        }
    }

    fn switch(&mut self, i: usize) {
        let t = self.switches[i].value;
        let (x, y, w, h) = switch_rect(i);
        let id = Transform::identity();
        let off = (255.0 * 0.2, 255.0 * 0.2, 255.0 * 0.2);
        let (r, g, b) = ACCENT;
        let lerp = |a: f32, b: u8| (a + (b as f32 - a) * t.clamp(0.0, 1.0)) as u8;
        let paint = draw::color(lerp(off.0, r), lerp(off.1, g), lerp(off.2, b), 1.0);
        draw::fill(&mut self.pix, &draw::rrect(x, y, w, h, h / 2.0), &paint, id);
        let kr = h / 2.0 - 2.0;
        let kx = x + h / 2.0 + t * (w - h);
        draw::shadow_n(&mut self.pix, kx - kr, y + 2.0, 2.0 * kr, 2.0 * kr, kr, 0.8, id, 3);
        draw::circle(&mut self.pix, kx, y + h / 2.0, kr, &draw::color(255, 255, 255, 1.0));
    }

    pub fn render(&mut self, ctx: &mut Ctx) -> R<()> {
        if !self.visible {
            return Ok(());
        }
        let s = ctx.settings;
        let id = Transform::identity();
        self.pix.fill(Color::from_rgba8(38, 38, 42, 255));

        // Tamanho
        self.label(ctx, "Tamanho", LABEL_X, ROW_SIZE, 16.0, 0.92);
        self.slider(Slider::Size, s.icon_size, (ICON_MIN, ICON_MAX), 1.0);
        self.label(ctx, "Pequeno", CTRL_X - 2.0, ROW_SIZE + 24.0, 13.0, 0.5);
        self.label_right(ctx, "Grande", CTRL_END + 2.0, ROW_SIZE + 24.0, 13.0, 0.5);

        // Ampliação
        self.label(ctx, "Ampliação", LABEL_X, ROW_MAG, 16.0, 0.92);
        self.switch(0);
        let en = self.switches[0].value.clamp(0.0, 1.0);
        self.slider(Slider::Mag, s.max_size, (MAG_MIN, MAG_MAX), en);
        self.label(ctx, "Mín.", MAG_SLIDER_X - 2.0, ROW_MAG + 24.0, 13.0, 0.25 + 0.25 * en);
        self.label_right(ctx, "Máx.", CTRL_END + 2.0, ROW_MAG + 24.0, 13.0, 0.25 + 0.25 * en);

        // Posição na tela (controle segmentado com seleção deslizante)
        self.label(ctx, "Posição na tela", LABEL_X, ROW_EDGE, 16.0, 0.92);
        self.segmented(ctx, ROW_EDGE, ["Esquerda", "Embaixo", "Direita"], self.segment.value);

        // Tamanho do texto
        self.label(ctx, "Tamanho do texto", LABEL_X, ROW_TEXT, 16.0, 0.92);
        self.slider(Slider::Text, s.text_scale, (TEXT_MIN, TEXT_MAX), 1.0);
        let pct = format!("{}%", (s.text_scale * 100.0).round());
        self.label(ctx, "A", CTRL_X - 2.0, ROW_TEXT + 24.0, 11.0, 0.5);
        self.label(ctx, &pct, (CTRL_X + CTRL_END) / 2.0 - 16.0, ROW_TEXT + 24.0, 12.0, 0.6);
        self.label_right(ctx, "A", CTRL_END + 2.0, ROW_TEXT + 24.0, 17.0, 0.5);

        // Tema
        self.label(ctx, "Tema", LABEL_X, ROW_THEME, 16.0, 0.92);
        let (tx, ty, tw, th) = segment_rect(ROW_THEME);
        draw::fill(&mut self.pix, &draw::rrect(tx, ty, tw, th, 8.0), &draw::color(255, 255, 255, 0.09), id);
        for bx in [tx + 2.0, tx + tw - THEME_BTN - 2.0] {
            draw::fill(&mut self.pix, &draw::rrect(bx, ty + 2.0, THEME_BTN, th - 4.0, 6.0), &draw::color(100, 100, 106, 1.0), id);
        }
        self.label(ctx, "‹", tx + 2.0 + THEME_BTN / 2.0 - 5.0, ROW_THEME - 1.0, 20.0, 0.95);
        self.label(ctx, "›", tx + tw - THEME_BTN / 2.0 - 7.0, ROW_THEME - 1.0, 20.0, 0.95);
        let n = ctx.themes.len();
        let cur = ctx.themes.iter().position(|t| t.name == ctx.theme).unwrap_or(0);
        let name = format!("{}   ({}/{})", ctx.theme, cur + 1, n);
        let nw = ctx.text.width(&name, Self::fit(ctx, 15.0));
        self.label(ctx, &name, tx + (tw - nw) / 2.0 - 2.0, ROW_THEME, 15.0, 0.95);
        let tp = ctx.text.get("Abrir pasta de temas", Self::fit(ctx, 13.0), 0x4da3ff);
        draw::blit(&mut self.pix, tp, Transform::from_translate(tx - 2.0, ty + th + 3.0), 1.0);

        if let Some(r) = tiny_skia::Rect::from_xywh(LABEL_X, DIVIDER, W - 2.0 * LABEL_X, 1.0) {
            self.pix.fill_rect(r, &draw::color(255, 255, 255, 0.1), id, None);
        }

        // Ocultar: nunca, sempre (sem o mouse) ou inteligente (quando a janela ativa encosta)
        self.label(ctx, "Ocultar a Dock", LABEL_X, ROW_HIDE, 16.0, 0.92);
        self.segmented(ctx, ROW_HIDE, ["Nunca", "Sempre", "Inteligente"], self.hide_segment.value);

        let toggles = [
            "Animar a abertura de aplicativos",
            "Mostrar indicadores de aplicativos abertos",
            "Prévia de janelas ao passar o mouse",
            "Destacar apps compatíveis ao arrastar",
            "Arquivos recentes no menu do app",
        ];
        for (i, l) in toggles.iter().enumerate() {
            self.label(ctx, l, LABEL_X, ROW_TOGGLES[i], 16.0, 0.92);
            self.switch(i + 1);
        }

        self.out.put(&ctx.conn, self.win, &self.pix, (0, 0, W as i32, H as i32))
    }
}
