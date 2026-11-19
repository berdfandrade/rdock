//! Prévia de janelas: com o mouse parado sobre um app aberto, miniaturas de cada janela.
//!
//! As miniaturas vêm de GetImage numa thread separada (com compositor, o servidor guarda o
//! conteúdo mesmo de janelas cobertas) e são atualizadas a cada segundo só enquanto o
//! popover está aberto. Clique traz a janela; o × fecha sem trazê-la para a frente.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use tiny_skia::{Pixmap, Transform};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

use crate::anim::Spring;
use crate::config::Edge;
use crate::draw;
use crate::stack::Anchor;
use crate::{x, Ctx, Msg, R};

const CW: f32 = 232.0;
const CH: f32 = 136.0;
const GAP: f32 = 12.0;
const PAD: f32 = 16.0;
const HEAD: f32 = 46.0;
const LABEL: f32 = 30.0;
const M: f32 = 30.0;
const ACCENT: (u8, u8, u8) = (10, 132, 255);
const HEAD_FONT: f32 = 15.0;
const BTN_FONT: f32 = 14.0;
const NEW_LABEL: &str = "+  Nova janela";
const REFRESH: Duration = Duration::from_millis(1000);
const LEAVE_DELAY: Duration = Duration::from_millis(280);

struct Win {
    id: Window,
    title: String,
    desktop: Option<u32>,
    thumb: Option<Pixmap>,
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Card(usize),
    Close(usize),
    NewWindow,
    Inside,
}

pub struct Previews {
    pub win: Window,
    out: x::Surface,
    pix: Pixmap,
    pub visible: bool,
    pub closing: bool,
    pub app: usize,
    anchor: Anchor,
    rect: (f32, f32, f32, f32),
    cw: f32,
    /// Margem esquerda dos cartões (centralizados quando o cabeçalho é mais largo).
    cards_x: f32,
    wins: Vec<Win>,
    t: Spring,
    hover: Option<Hit>,
    pub inside: bool,
    leave_at: Option<Instant>,
    last_refresh: Instant,
    bg: Option<Pixmap>,
    tint: f32,
    tx: Sender<Msg>,
    reduce_motion: bool,
    pub needs_frame: bool,
}

impl Previews {
    pub fn new(ctx: &mut Ctx, tx: Sender<Msg>) -> R<Previews> {
        let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::ENTER_WINDOW | EventMask::LEAVE_WINDOW;
        let (win, out) = x::create_window(&ctx.conn, ctx.root, ctx.visual, ctx.colormap, (0, 0, 10, 10), true, events)?;
        ctx.own.push(win);
        Ok(Previews {
            win,
            out,
            pix: Pixmap::new(10, 10).ok_or("pixmap")?,
            visible: false,
            closing: false,
            app: usize::MAX,
            anchor: Anchor { icon: (0.0, 0.0), edge_pt: (0.0, 0.0), edge: Edge::Bottom },
            rect: (0.0, 0.0, 10.0, 10.0),
            cw: CW,
            cards_x: PAD,
            wins: Vec::new(),
            t: Spring::new(0.0, 340.0, 0.85),
            hover: None,
            inside: false,
            leave_at: None,
            last_refresh: Instant::now(),
            bg: None,
            tint: 0.7,
            tx,
            reduce_motion: ctx.reduce_motion,
            needs_frame: false,
        })
    }

    pub fn open(&mut self, ctx: &mut Ctx, app: usize, anchor: Anchor) -> R<()> {
        let Some(list) = ctx.running.get(&app) else { return Ok(()) };
        let a = ctx.atoms;
        self.wins = list
            .iter()
            .map(|&id| {
                let title = x::text_prop(&ctx.conn, id, a._NET_WM_NAME, a.UTF8_STRING).or_else(|| x::text_prop(&ctx.conn, id, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into())).unwrap_or_default();
                let desktop = x::prop32(&ctx.conn, id, a._NET_WM_DESKTOP, AtomEnum::CARDINAL).first().copied().filter(|&d| d != u32::MAX);
                Win { id, title, desktop, thumb: None }
            })
            .collect();
        self.app = app;
        self.anchor = anchor;
        let n = self.wins.len() as f32;
        let (mx, my, mw, mh) = (ctx.mon.0 as f32, ctx.mon.1 as f32, ctx.mon.2 as f32, ctx.mon.3 as f32);
        // Muitas janelas: os cartões encolhem para caber na tela.
        self.cw = CW.min((mw - 2.0 * M - 2.0 * PAD - (n - 1.0) * GAP) / n).max(120.0);
        let ch = self.cw * CH / CW;
        // O cabeçalho (nome · janelas + botão) também precisa caber: com uma janela só, ele é o mais largo.
        let cards = n * self.cw + (n - 1.0) * GAP;
        let head_w = ctx.text.width(&self.head(ctx), HEAD_FONT) + 20.0 + ctx.text.width(NEW_LABEL, BTN_FONT) + 24.0;
        let pw = (2.0 * PAD + cards.max(head_w)).min(mw - 2.0 * M);
        self.cards_x = (pw - cards) / 2.0;
        let ph = HEAD + ch + LABEL + PAD;
        let (px, py) = match anchor.edge {
            Edge::Bottom => (anchor.icon.0 - pw / 2.0, anchor.edge_pt.1 - 12.0 - ph),
            Edge::Left => (anchor.edge_pt.0 + 12.0, anchor.icon.1 - ph / 2.0),
            Edge::Right => (anchor.edge_pt.0 - 12.0 - pw, anchor.icon.1 - ph / 2.0),
        };
        let px = px.clamp(mx + 8.0, (mx + mw - pw - 8.0).max(mx + 8.0)).round();
        let py = py.clamp(my + 8.0, (my + mh - ph - 8.0).max(my + 8.0)).round();
        if !self.visible {
            if let Some((bg, lum)) = crate::launchpad::capture_blurred(ctx, (px as i32, py as i32, pw as u32, ph as u32), 0.9) {
                self.bg = Some(bg);
                self.tint = 0.5 + 0.38 * lum;
            }
        }
        let (x, y, w, h) = (px - M, py - M, pw + 2.0 * M, ph + 2.0 * M);
        self.rect = (x, y, w, h);
        ctx.conn.configure_window(self.win, &ConfigureWindowAux::new().x(x as i32).y(y as i32).width(w as u32).height(h as u32).stack_mode(StackMode::ABOVE))?;
        if self.pix.width() != w as u32 || self.pix.height() != h as u32 {
            self.pix = Pixmap::new(w as u32, h as u32).ok_or("pixmap")?;
        }
        if !self.visible {
            ctx.conn.map_window(self.win)?;
            self.t.snap(0.0);
        }
        self.t.target = 1.0;
        self.visible = true;
        self.closing = false;
        self.inside = false;
        self.leave_at = None;
        self.capture(ctx);
        self.needs_frame = true;
        Ok(())
    }

    pub fn close(&mut self) {
        if self.visible && !self.closing {
            self.closing = true;
            self.t.target = 0.0;
            self.needs_frame = true;
        }
    }

    /// Captura as janelas numa thread; as miniaturas chegam como `Msg::Thumb`.
    fn capture(&mut self, ctx: &Ctx) {
        self.last_refresh = Instant::now();
        let ids: Vec<Window> = self.wins.iter().map(|w| w.id).collect();
        let (conn, tx) = (ctx.conn.clone(), self.tx.clone());
        let (tw, th) = (self.cw as u32 * 2, (self.cw * CH / CW) as u32 * 2);
        std::thread::spawn(move || {
            for id in ids {
                if let Some(p) = grab(&conn, id, tw, th) {
                    if tx.send(Msg::Thumb(id, p)).is_err() {
                        return;
                    }
                }
            }
        });
    }

    pub fn on_thumb(&mut self, id: Window, pix: Pixmap) {
        if let Some(w) = self.wins.iter_mut().find(|w| w.id == id) {
            w.thumb = Some(pix);
            self.needs_frame = self.visible;
        }
    }

    /// Fecha quando o mouse sai do ícone e do popover (com uma pequena tolerância).
    pub fn update(&mut self, ctx: &Ctx, dock_hover: Option<usize>) {
        if !self.visible || self.closing {
            return;
        }
        if self.inside || dock_hover == Some(self.app) {
            self.leave_at = None;
        } else if self.leave_at.get_or_insert_with(Instant::now).elapsed() >= LEAVE_DELAY {
            self.close();
        } else {
            self.needs_frame = true;
        }
        if self.last_refresh.elapsed() >= REFRESH {
            self.capture(ctx);
        }
    }

    fn head(&self, ctx: &Ctx) -> String {
        let n = ctx.running.get(&self.app).map_or(0, |w| w.len());
        let name = ctx.apps.get(self.app).map(|a| a.name.clone()).unwrap_or_default();
        format!("{name} · {n} {}", if n == 1 { "janela" } else { "janelas" })
    }

    fn card_rect(&self, k: usize) -> (f32, f32, f32, f32) {
        (M + self.cards_x + k as f32 * (self.cw + GAP), M + HEAD, self.cw, self.cw * CH / CW)
    }

    fn new_btn(&self, ctx: &Ctx) -> (f32, f32, f32, f32) {
        let w = ctx.text.width(NEW_LABEL, BTN_FONT) + 24.0;
        (self.rect.2 - M - PAD - w, M + 9.0, w, 30.0)
    }

    fn hit(&self, ctx: &Ctx, x: f32, y: f32) -> Option<Hit> {
        let inr = |r: (f32, f32, f32, f32)| x >= r.0 && x <= r.0 + r.2 && y >= r.1 && y <= r.1 + r.3;
        if inr(self.new_btn(ctx)) {
            return Some(Hit::NewWindow);
        }
        for k in 0..self.wins.len() {
            let r = self.card_rect(k);
            if (x - (r.0 + 14.0)).hypot(y - (r.1 + 14.0)) <= 12.0 {
                return Some(Hit::Close(k));
            }
            if inr((r.0, r.1, r.2, r.3 + LABEL)) {
                return Some(Hit::Card(k));
            }
        }
        inr((M, M, self.rect.2 - 2.0 * M, self.rect.3 - 2.0 * M)).then_some(Hit::Inside)
    }

    pub fn on_motion(&mut self, ctx: &Ctx, x: f32, y: f32) {
        let h = self.hit(ctx, x, y);
        if h != self.hover {
            self.hover = h;
            self.needs_frame = true;
        }
    }

    pub fn on_crossing(&mut self, inside: bool) {
        self.inside = inside;
        if !inside {
            self.hover = None;
        }
        self.needs_frame = true;
    }

    pub fn on_release(&mut self, ctx: &mut Ctx, button: u8, x: f32, y: f32) {
        if button == 4 || button == 5 {
            // A roda alterna entre as janelas do app.
            if let Some(k) = match self.hover {
                Some(Hit::Card(k)) => Some(k),
                _ => None,
            } {
                let n = self.wins.len();
                let k = if button == 4 { (k + n - 1) % n } else { (k + 1) % n };
                self.hover = Some(Hit::Card(k));
                x::activate(&ctx.conn, &ctx.atoms, ctx.root, self.wins[k].id, ctx.time);
                self.needs_frame = true;
            }
            return;
        }
        if button != 1 {
            return;
        }
        match self.hit(ctx, x, y) {
            Some(Hit::Card(k)) => {
                x::activate(&ctx.conn, &ctx.atoms, ctx.root, self.wins[k].id, ctx.time);
                self.close();
            }
            Some(Hit::Close(k)) => {
                x::close(&ctx.conn, &ctx.atoms, ctx.root, self.wins[k].id, ctx.time);
                self.close();
            }
            Some(Hit::NewWindow) => {
                crate::apps::launch(&ctx.apps[self.app]);
                self.close();
            }
            _ => {}
        }
    }

    pub fn tick(&mut self, ctx: &Ctx, dt: f32) -> bool {
        let anim = self.t.step(dt);
        if self.closing && self.t.value < 0.02 {
            let _ = ctx.conn.unmap_window(self.win);
            self.visible = false;
            self.closing = false;
            self.bg = None;
            self.wins.clear();
            return false;
        }
        anim || self.closing || self.leave_at.is_some()
    }

    pub fn render(&mut self, ctx: &mut Ctx) -> R<()> {
        if !self.visible {
            return Ok(());
        }
        let t = self.t.value;
        let a = t.clamp(0.0, 1.0);
        self.pix.fill(tiny_skia::Color::TRANSPARENT);
        let (w, h) = (self.rect.2, self.rect.3);
        let (pw, ph) = (w - 2.0 * M, h - 2.0 * M);
        let (ix, iy) = (self.anchor.icon.0 - self.rect.0, self.anchor.icon.1 - self.rect.1);
        let tip = match self.anchor.edge {
            Edge::Bottom => (ix.clamp(M + 24.0, w - M - 24.0), M + ph + 10.0),
            Edge::Left => (M - 10.0, iy.clamp(M + 24.0, h - M - 24.0)),
            Edge::Right => (M + pw + 10.0, iy.clamp(M + 24.0, h - M - 24.0)),
        };
        let s = if self.reduce_motion { 1.0 } else { 0.95 + 0.05 * t };
        let tf = Transform::from_translate(tip.0, tip.1).pre_scale(s, s).pre_translate(-tip.0, -tip.1);
        draw::glass(&mut self.pix, self.bg.as_ref(), (M, M), M, M, pw, ph, 18.0, self.tint, a, tf);
        let mut pb = tiny_skia::PathBuilder::new();
        let (dx, dy) = match self.anchor.edge {
            Edge::Bottom => (0.0, -1.0),
            Edge::Left => (1.0, 0.0),
            Edge::Right => (-1.0, 0.0),
        };
        pb.move_to(tip.0 + 10.0 * dx - 10.0 * dy, tip.1 + 10.0 * dy - 10.0 * dx);
        pb.line_to(tip.0, tip.1);
        pb.line_to(tip.0 + 10.0 * dx + 10.0 * dy, tip.1 + 10.0 * dy + 10.0 * dx);
        pb.close();
        draw::fill(&mut self.pix, &pb.finish(), &draw::color(34, 32, 40, 0.96 * a), tf);
        let ca = ((t - 0.3) / 0.7).clamp(0.0, 1.0);
        if ca <= 0.0 {
            return self.out.put(&ctx.conn, self.win, &self.pix, (0, 0, w as i32, h as i32));
        }
        let id = Transform::identity();
        let n = self.wins.len();
        // Nunca invade o botão "Nova janela".
        let room = self.new_btn(ctx).0 - (M + PAD) - 12.0;
        let head = ctx.text.ellipsize(&self.head(ctx), HEAD_FONT, room);
        let tp = ctx.text.get(&head, HEAD_FONT, 0xffffff);
        draw::blit_fast(&mut self.pix, tp, (M + PAD) as i32, (M + 24.0 - tp.height() as f32 / 2.0) as i32, ca);
        let r = self.new_btn(ctx);
        let hv = self.hover == Some(Hit::NewWindow);
        draw::fill(&mut self.pix, &draw::rrect(r.0, r.1, r.2, r.3, 9.0), &draw::color(255, 255, 255, if hv { 0.22 } else { 0.12 } * ca), id);
        let tp = ctx.text.get(NEW_LABEL, BTN_FONT, 0xffffff);
        draw::blit_fast(&mut self.pix, tp, (r.0 + 12.0) as i32, (r.1 + (r.3 - tp.height() as f32) / 2.0) as i32, ca);
        let icon = ctx.apps.get(self.app).map(|a| a.icon.clone()).unwrap_or_default();
        for k in 0..n {
            let (x, y, cw, ch) = self.card_rect(k);
            let hovered = matches!(self.hover, Some(Hit::Card(j) | Hit::Close(j)) if j == k);
            draw::fill(&mut self.pix, &draw::rrect(x, y, cw, ch, 9.0), &draw::color(18, 18, 22, 0.9 * ca), id);
            match &self.wins[k].thumb {
                Some(p) => {
                    let (sw, sh) = (p.width() as f32, p.height() as f32);
                    let k2 = (cw / sw).min(ch / sh);
                    let (dw, dh) = (sw * k2, sh * k2);
                    let tf = Transform::from_row(k2, 0.0, 0.0, k2, x + (cw - dw) / 2.0, y + (ch - dh) / 2.0);
                    draw::blit(&mut self.pix, p, tf, ca);
                }
                None => draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, &icon, x + cw / 2.0, y + ch / 2.0 - 28.0, 56.0, ca * 0.8, 0.0),
            }
            let ring = if hovered { draw::color(ACCENT.0, ACCENT.1, ACCENT.2, ca) } else { draw::color(255, 255, 255, 0.12 * ca) };
            draw::stroke(&mut self.pix, &draw::rrect(x + 0.5, y + 0.5, cw - 1.0, ch - 1.0, 9.0), &ring, if hovered { 2.0 } else { 1.0 }, id);
            if let Some(d) = self.wins[k].desktop {
                let lbl = format!("Área {}", d + 1);
                let tp = ctx.text.get(&lbl, 12.0, 0xffffff);
                let (bw, bh) = (tp.width() as f32 + 12.0, tp.height() as f32 + 2.0);
                let (bx, by) = (x + cw - bw - 6.0, y + ch - bh - 6.0);
                draw::fill(&mut self.pix, &draw::rrect(bx, by, bw, bh, 6.0), &draw::color(0, 0, 0, 0.6 * ca), id);
                let tp = ctx.text.get(&lbl, 12.0, 0xffffff);
                draw::blit_fast(&mut self.pix, tp, (bx + 6.0) as i32, (by + 1.0) as i32, ca);
            }
            if hovered {
                let close_hv = self.hover == Some(Hit::Close(k));
                draw::circle(&mut self.pix, x + 14.0, y + 14.0, 10.0, &draw::color(if close_hv { 230 } else { 70 }, if close_hv { 60 } else { 70 }, if close_hv { 50 } else { 76 }, 0.95 * ca));
                let tp = ctx.text.get("×", 14.5, 0xffffff);
                draw::blit_fast(&mut self.pix, tp, (x + 14.0 - tp.width() as f32 / 2.0) as i32, (y + 14.0 - tp.height() as f32 / 2.0) as i32, ca);
            }
            let title = ctx.text.ellipsize(&self.wins[k].title, 13.5, cw - 8.0);
            let tp = ctx.text.get(&title, 13.5, 0xffffff);
            draw::blit_fast(&mut self.pix, tp, (x + (cw - tp.width() as f32) / 2.0) as i32, (y + ch + 7.0) as i32, if hovered { ca } else { 0.75 * ca });
        }
        self.out.put(&ctx.conn, self.win, &self.pix, (0, 0, w as i32, h as i32))
    }
}

/// Captura a janela e reduz para caber em tw×th (média de blocos), já em RGBA opaco.
fn grab(conn: &x11rb::rust_connection::RustConnection, id: Window, tw: u32, th: u32) -> Option<Pixmap> {
    let attrs = conn.get_window_attributes(id).ok()?.reply().ok()?;
    if attrs.map_state != MapState::VIEWABLE {
        return None;
    }
    let g = conn.get_geometry(id).ok()?.reply().ok()?;
    let (w, h) = (g.width as u32, g.height as u32);
    if w < 2 || h < 2 {
        return None;
    }
    let img = conn.get_image(ImageFormat::Z_PIXMAP, id, 0, 0, w as u16, h as u16, !0).ok()?.reply().ok()?;
    let _ = conn.flush();
    if img.data.len() < (w * h * 4) as usize {
        return None;
    }
    let k = (tw as f32 / w as f32).min(th as f32 / h as f32).min(1.0);
    let (dw, dh) = (((w as f32 * k) as u32).max(1), ((h as f32 * k) as u32).max(1));
    let mut out = Pixmap::new(dw, dh)?;
    let (bx, by) = ((w / dw).max(1), (h / dh).max(1));
    // Amostra no máximo 4×4 pixels por bloco: rápido e sem serrilhado visível.
    let (sx, sy) = ((bx / 4).max(1), (by / 4).max(1));
    for (i, px) in out.data_mut().chunks_exact_mut(4).enumerate() {
        let (ox, oy) = ((i as u32 % dw) * w / dw, (i as u32 / dw) * h / dh);
        let mut acc = [0u32; 3];
        let mut n = 0;
        let mut yy = oy;
        while yy < (oy + by).min(h) {
            let mut xx = ox;
            while xx < (ox + bx).min(w) {
                let o = ((yy * w + xx) * 4) as usize;
                acc[0] += img.data[o + 2] as u32;
                acc[1] += img.data[o + 1] as u32;
                acc[2] += img.data[o] as u32;
                n += 1;
                xx += sx;
            }
            yy += sy;
        }
        let n = n.max(1);
        px.copy_from_slice(&[(acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8, 255]);
    }
    Some(out)
}
