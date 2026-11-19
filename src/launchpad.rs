//! Launchpad: grade em tela cheia com todos os apps instalados, fundo desfocado,
//! zoom de entrada/saída, páginas deslizantes e busca por teclado.

use std::time::{Duration, Instant};

use tiny_skia::{Color, FilterQuality, Pixmap, PixmapPaint, Transform};
use x11rb::protocol::xproto::*;

use crate::anim::{ease_out_cubic, Spring};
use crate::draw;
use crate::menu::Menu;
use crate::x::{self, Keymap};
use crate::{apps, Action, Ctx, R};

const COLS: usize = 7;
const ROWS: usize = 5;
const PER: usize = COLS * ROWS;
const ICON: f32 = 92.0;
const LABEL: f32 = 15.0;
const OPEN_SECS: f32 = 0.32;
const CLOSE_SECS: f32 = 0.22;

struct Press {
    hit: Option<usize>,
    x: f32,
    y: f32,
    pos0: f32,
    dragging: bool,
}

pub struct Launchpad {
    pub win: Window,
    out: x::Surface,
    pix: Pixmap,
    w: f32,
    h: f32,
    pub visible: bool,
    pub closing: bool,
    /// Progresso linear da animação de abrir (0 → 1) ou fechar (1 → 0).
    t: f32,
    bg: Option<Pixmap>,
    query: String,
    list: Vec<usize>,
    /// Nomes já encurtados com "…" para cada item de `list` (vazio = recalcular).
    labels: Vec<String>,
    labels_scale: f32,
    page: i32,
    pos: Spring,
    press: Option<Press>,
    menu: Option<Menu>,
    last_scroll: Instant,
    keymap: Keymap,
    /// Canto do monitor na tela (para converter coordenadas ao arrastar para a dock).
    ox: f32,
    oy: f32,
    pub needs_frame: bool,
}

impl Launchpad {
    pub fn new(ctx: &mut Ctx) -> R<Launchpad> {
        let (_, _, mw, mh) = ctx.mon;
        let events = EventMask::EXPOSURE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::KEY_PRESS;
        let (win, out) = x::create_window(&ctx.conn, ctx.root, ctx.visual, ctx.colormap, ctx.mon, true, events)?;
        ctx.own.push(win);
        Ok(Launchpad {
            win,
            out,
            pix: Pixmap::new(mw as u32, mh as u32).ok_or("pixmap")?,
            w: mw as f32,
            h: mh as f32,
            visible: false,
            closing: false,
            t: 0.0,
            bg: None,
            query: String::new(),
            list: Vec::new(),
            labels: Vec::new(),
            labels_scale: 0.0,
            page: 0,
            pos: Spring::new(0.0, 230.0, 0.9),
            press: None,
            menu: None,
            last_scroll: Instant::now(),
            keymap: Keymap::load(&ctx.conn)?,
            ox: ctx.mon.0 as f32,
            oy: ctx.mon.1 as f32,
            needs_frame: false,
        })
    }

    pub fn open(&mut self, ctx: &mut Ctx) -> R<()> {
        if !self.visible {
            self.bg = capture_blurred(ctx, (ctx.mon.0, ctx.mon.1, self.w as u32, self.h as u32), 0.7).map(|b| b.0);
            self.query.clear();
            self.refilter(ctx);
            self.page = 0;
            self.pos.snap(0.0);
            self.t = 0.0;
            ctx.conn.map_window(self.win)?;
            ctx.conn.configure_window(self.win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        }
        self.menu = None;
        self.press = None;
        ctx.conn.set_input_focus(InputFocus::PARENT, self.win, x11rb::CURRENT_TIME)?;
        for _ in 0..20 {
            let ok = ctx
                .conn
                .grab_keyboard(false, self.win, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
                .reply()
                .is_ok_and(|r| r.status == GrabStatus::SUCCESS);
            if ok {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.visible = true;
        self.closing = false;
        self.needs_frame = true;
        Ok(())
    }

    pub fn close(&mut self, ctx: &mut Ctx) {
        if !self.visible || self.closing {
            return;
        }
        self.closing = true;
        self.menu = None;
        let _ = ctx.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        self.needs_frame = true;
    }

    fn refilter(&mut self, ctx: &Ctx) {
        let q = apps::normalize(self.query.trim());
        let mut v: Vec<(u8, usize)> = ctx
            .apps
            .iter()
            .enumerate()
            .filter(|(_, a)| a.listed)
            .filter_map(|(i, a)| {
                if q.is_empty() {
                    return Some((0, i));
                }
                let name = apps::normalize(&a.name);
                if name.starts_with(&q) {
                    Some((0, i))
                } else if name.split_whitespace().any(|w| w.starts_with(&q)) {
                    Some((1, i))
                } else if a.search.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&q)) {
                    Some((2, i))
                } else {
                    None
                }
            })
            .collect();
        v.sort_by_key(|&(rank, i)| (rank, i));
        self.list = v.into_iter().map(|(_, i)| i).collect();
        self.labels.clear();
        self.page = 0;
        self.pos.target = 0.0;
        self.needs_frame = true;
    }

    fn pages(&self) -> i32 {
        self.list.len().div_ceil(PER).max(1) as i32
    }

    fn go_page(&mut self, p: i32) {
        self.page = p.clamp(0, self.pages() - 1);
        self.pos.target = self.page as f32;
        self.needs_frame = true;
    }

    fn grid(&self) -> (f32, f32, f32, f32) {
        let cell_w = (self.w * 0.72 / COLS as f32).clamp(120.0, 205.0);
        let cell_h = ((self.h - 250.0) / ROWS as f32).clamp(120.0, 175.0);
        let x0 = (self.w - cell_w * COLS as f32) / 2.0;
        let y0 = 100.0 + ((self.h - 230.0) - cell_h * ROWS as f32) / 2.0;
        (cell_w, cell_h, x0, y0)
    }

    /// Centro horizontal e topo do ícone `i` (índice em `list`), na posição de página `pos`.
    fn icon_pos(&self, i: usize, pos: f32) -> (f32, f32) {
        let (cw, ch, x0, y0) = self.grid();
        let (page, k) = (i / PER, i % PER);
        let (col, row) = (k % COLS, k / COLS);
        let cx = x0 + (col as f32 + 0.5) * cw + (page as f32 - pos) * self.w;
        (cx, y0 + row as f32 * ch + 8.0)
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let page = self.pos.value.round().max(0.0) as usize;
        let start = page * PER;
        (start..(start + PER).min(self.list.len())).find(|&i| {
            let (cx, top) = self.icon_pos(i, self.pos.value);
            (x - cx).abs() < ICON / 2.0 + 10.0 && y > top - 6.0 && y < top + ICON + 26.0
        })
    }

    fn search_rect(&self, text_scale: f32) -> (f32, f32, f32, f32) {
        (((self.w - 320.0) / 2.0).round(), 36.0, 320.0, (24.0 + 13.0 * text_scale).round())
    }

    // ---------------------------------------------------------------- entrada

    pub fn on_motion(&mut self, x: f32, y: f32) -> Option<Action> {
        if let Some(m) = &mut self.menu {
            m.hovered = m.hit(x, y);
            self.needs_frame = true;
            return None;
        }
        let max = (self.pages() - 1) as f32;
        let (ox, oy) = (self.ox, self.oy);
        let p = self.press.as_mut()?;
        let dx = x - p.x;
        // Arrastar um ícone o leva para a dock; arrastar o fundo troca de página.
        if !p.dragging && (dx).hypot(y - p.y) > 10.0 {
            if let Some(i) = p.hit {
                let app = self.list[i];
                self.press = None;
                return Some(Action::DragToDock(app, x + ox, y + oy));
            }
        }
        if !p.dragging && dx.abs() > 10.0 {
            p.dragging = true;
        }
        if p.dragging {
            let mut v = p.pos0 - dx / self.w;
            if v < 0.0 {
                v *= 0.35;
            } else if v > max {
                v = max + (v - max) * 0.35;
            }
            self.pos.snap(v);
            self.needs_frame = true;
        }
        None
    }

    pub fn on_press(&mut self, ctx: &mut Ctx, button: u8, x: f32, y: f32) {
        self.needs_frame = true;
        if self.menu.is_some() {
            if self.menu.as_ref().and_then(|m| m.hit(x, y)).is_none() {
                self.menu = None;
            }
            return;
        }
        match button {
            1 => {
                self.press = Some(Press { hit: self.hit(x, y), x, y, pos0: self.pos.value, dragging: false });
            }
            3 => {
                if let Some(i) = self.hit(x, y) {
                    let app = self.list[i];
                    let pin = if ctx.pinned.contains(&app) { "Remover do Dock" } else { "Manter no Dock" };
                    let items = vec![("Abrir".to_string(), Action::Launch(app)), (pin.to_string(), Action::TogglePin(app))];
                    let (cx, top) = self.icon_pos(i, self.pos.value);
                    let mut m = Menu::new(items, &ctx.text);
                    let (w, h) = m.size();
                    m.place(cx - w / 2.0, top - 6.0 - h, (cx, top - 6.0), (self.w, self.h));
                    self.menu = Some(m);
                }
            }
            4..=7 if self.last_scroll.elapsed() > Duration::from_millis(320) => {
                self.last_scroll = Instant::now();
                let d = if button == 4 || button == 6 { -1 } else { 1 };
                self.go_page(self.page + d);
            }
            _ => {}
        }
    }

    pub fn on_release(&mut self, ctx: &mut Ctx, button: u8, x: f32, y: f32) -> Option<Action> {
        if let Some(m) = &self.menu {
            let act = m.hit(x, y).and_then(|k| m.action(k));
            if act.is_some() {
                self.menu = None;
                self.needs_frame = true;
            }
            return act;
        }
        if button != 1 {
            return None;
        }
        let p = self.press.take()?;
        self.needs_frame = true;
        if p.dragging {
            let dx = x - p.x;
            let delta = if dx < -self.w * 0.1 { 1 } else if dx > self.w * 0.1 { -1 } else { 0 };
            self.go_page(p.pos0.round() as i32 + delta);
            return None;
        }
        let hit = self.hit(x, y);
        if hit.is_some() && hit == p.hit {
            return hit.map(|i| Action::Launch(self.list[i]));
        }
        let (sx, sy, sw, sh) = self.search_rect(ctx.text.scale);
        let in_search = x >= sx && x <= sx + sw && y >= sy && y <= sy + sh;
        if hit.is_none() && p.hit.is_none() && !in_search && y < self.h - 90.0 {
            self.close(ctx);
        }
        None
    }

    pub fn on_key(&mut self, ctx: &mut Ctx, code: u8, state: KeyButMask) -> Option<Action> {
        let shift = u16::from(state) & u16::from(KeyButMask::SHIFT) != 0;
        let ks = self.keymap.keysym(code, shift);
        self.needs_frame = true;
        match ks {
            0xff1b => {
                if self.menu.take().is_none() {
                    if self.query.is_empty() {
                        self.close(ctx);
                    } else {
                        self.query.clear();
                        self.refilter(ctx);
                    }
                }
            }
            0xff08 => {
                self.query.pop();
                self.refilter(ctx);
            }
            0xff0d | 0xff8d => {
                let i = self.page as usize * PER;
                return self.list.get(i).or(self.list.first()).map(|&a| Action::Launch(a));
            }
            0xff51 | 0xff55 => self.go_page(self.page - 1),
            0xff53 | 0xff56 => self.go_page(self.page + 1),
            _ => {
                if let Some(c) = x::keysym_char(ks).filter(|c| !c.is_control()) {
                    if self.query.chars().count() < 40 {
                        self.query.push(c);
                        self.refilter(ctx);
                    }
                }
            }
        }
        None
    }

    // ---------------------------------------------------------------- animação e desenho

    pub fn tick(&mut self, ctx: &mut Ctx, dt: f32) -> bool {
        let t0 = self.t;
        self.t = if self.closing { self.t - dt / CLOSE_SECS } else { self.t + dt / OPEN_SECS }.clamp(0.0, 1.0);
        let mut anim = self.t != t0;
        anim |= self.pos.step(dt);
        if let Some(m) = &mut self.menu {
            anim |= m.anim.step(dt);
        }
        if self.closing && self.t <= 0.0 {
            let _ = ctx.conn.unmap_window(self.win);
            self.visible = false;
            self.closing = false;
            self.bg = None;
            return false;
        }
        anim || self.closing
    }

    pub fn render(&mut self, ctx: &mut Ctx) -> R<()> {
        if !self.visible {
            return Ok(());
        }
        // Abrir desacelera no fim; fechar acelera (sai rápido).
        let t = if self.closing { 1.0 - ease_out_cubic(1.0 - self.t) } else { ease_out_cubic(self.t) };
        match &self.bg {
            Some(bg) => draw::copy_faded(&mut self.pix, bg, t),
            None => self.pix.fill(Color::from_rgba8(18, 18, 22, (t * 225.0) as u8)),
        }

        // Campo de busca
        let (sx, sy, sw, sh) = self.search_rect(ctx.text.scale);
        let id = Transform::identity();
        draw::fill(&mut self.pix, &draw::rrect(sx, sy, sw, sh, 8.0), &draw::color(255, 255, 255, 0.13 * t), id);
        draw::stroke(&mut self.pix, &draw::rrect(sx + 0.5, sy + 0.5, sw - 1.0, sh - 1.0, 7.5), &draw::color(255, 255, 255, 0.2 * t), 1.0, id);
        let lens = tiny_skia::PathBuilder::from_circle(sx + 16.0, sy + 14.0, 5.0);
        draw::stroke(&mut self.pix, &lens, &draw::color(255, 255, 255, 0.7 * t), 1.6, id);
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(sx + 19.8, sy + 17.8);
        pb.line_to(sx + 23.5, sy + 21.5);
        draw::stroke(&mut self.pix, &pb.finish(), &draw::color(255, 255, 255, 0.7 * t), 1.8, id);
        let (label, alpha) = if self.query.is_empty() { ("Buscar", 0.5) } else { (self.query.as_str(), 1.0) };
        let tp = ctx.text.get(label, 16.0, 0xffffff);
        let ty = sy + (sh - tp.height() as f32) / 2.0;
        draw::blit_fast(&mut self.pix, tp, (sx + 29.0) as i32, ty.round() as i32, alpha * t);
        if !self.query.is_empty() {
            let caret_x = sx + 31.0 + ctx.text.width(&self.query, 16.0);
            if let Some(r) = tiny_skia::Rect::from_xywh(caret_x, sy + 7.0, 1.5, sh - 14.0) {
                self.pix.fill_rect(r, &draw::color(255, 255, 255, 0.9 * t), id, None);
            }
        }

        // Grade de ícones (só as páginas visíveis). O zoom de entrada afasta os ícones
        // do centro (110% → 100%) sem redimensioná-los: ícones e rótulos são copiados
        // em tamanho fixo, o que mantém cada quadro barato.
        let (cw, _, _, _) = self.grid();
        if self.labels.len() != self.list.len() || self.labels_scale != ctx.text.scale {
            self.labels = self.list.iter().map(|&a| ctx.text.ellipsize(&ctx.apps[a].name, LABEL, cw - 14.0)).collect();
            self.labels_scale = ctx.text.scale;
        }
        let z = 1.0 + 0.1 * (1.0 - t);
        let (mx, my) = (self.w / 2.0, self.h / 2.0);
        let pressed = self.press.as_ref().filter(|p| !p.dragging).and_then(|p| p.hit);
        for i in 0..self.list.len() {
            if ((i / PER) as f32 - self.pos.value).abs() >= 1.0 {
                continue;
            }
            let (icx, top) = self.icon_pos(i, self.pos.value);
            if icx < -cw || icx > self.w + cw {
                continue;
            }
            let (icx, top) = (mx + (icx - mx) * z, my + (top - my) * z);
            let app = &ctx.apps[self.list[i]];
            let dark = if pressed == Some(i) { 0.7 } else { 0.0 };
            draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, &app.icon, icx, top, ICON, t, dark);
            let name = &self.labels[i];
            let shadow = ctx.text.get(name, LABEL, 0x000000);
            let (lx, ly) = ((icx - shadow.width() as f32 / 2.0).round() as i32, (top + ICON + 6.0).round() as i32);
            draw::blit_fast(&mut self.pix, shadow, lx, ly + 1, 0.55 * t);
            draw::blit_fast(&mut self.pix, ctx.text.get(name, LABEL, 0xffffff), lx, ly, t);
        }
        if self.list.is_empty() {
            let tp = ctx.text.get("Nenhum resultado", 20.0, 0xffffff);
            let x = ((self.w - tp.width() as f32) / 2.0) as i32;
            draw::blit_fast(&mut self.pix, tp, x, (self.h * 0.42) as i32, 0.6 * t);
        }

        // Indicadores de página
        let n = self.pages();
        if n > 1 {
            let spacing = 18.0;
            let start = (self.w - (n - 1) as f32 * spacing) / 2.0;
            for k in 0..n {
                let near = (1.0 - (self.pos.value - k as f32).abs()).max(0.0);
                let a = (0.32 + 0.68 * near) * t;
                draw::circle(&mut self.pix, start + k as f32 * spacing, self.h - 62.0, 3.6, &draw::color(255, 255, 255, a));
            }
        }

        if let Some(m) = &self.menu {
            m.draw(&mut self.pix, &mut ctx.text);
        }
        self.out.put(&ctx.conn, self.win, &self.pix, (0, 0, self.w as i32, self.h as i32))
    }

    pub fn damage(&mut self) {
        if self.visible {
            self.needs_frame = true;
        }
    }
}

/// Captura a tela, reduz 8×, aplica desfoque e escurece: vira o fundo "vidro fosco".
/// Captura a região (x, y, w, h) da tela, reduz 8×, desfoca e escurece por `dim`.
/// Devolve o fundo "vidro fosco" e o brilho médio (0 a 1) da região.
pub fn capture_blurred(ctx: &Ctx, (mx, my, w, h): (i32, i32, u32, u32), dim: f32) -> Option<(Pixmap, f32)> {
    let r = ctx
        .conn
        .get_image(ImageFormat::Z_PIXMAP, ctx.root, mx as i16, my as i16, w as u16, h as u16, !0)
        .ok()?
        .reply()
        .ok()?;
    let (w, h) = (w as usize, h as usize);
    if r.data.len() < w * h * 4 {
        return None;
    }
    const F: usize = 8;
    let (sw, sh) = (w / F, h / F);
    let mut img = vec![[0f32; 3]; sw * sh];
    for sy in 0..sh {
        for sx in 0..sw {
            let mut acc = [0u32; 3];
            for yy in 0..F {
                let row = (sy * F + yy) * w * 4;
                for xx in 0..F {
                    let o = row + (sx * F + xx) * 4;
                    acc[0] += r.data[o + 2] as u32;
                    acc[1] += r.data[o + 1] as u32;
                    acc[2] += r.data[o] as u32;
                }
            }
            let n = (F * F) as f32;
            img[sy * sw + sx] = [acc[0] as f32 / n, acc[1] as f32 / n, acc[2] as f32 / n];
        }
    }
    for _ in 0..3 {
        box_blur(&mut img, sw, sh, 3);
    }
    let mut small = Pixmap::new(sw as u32, sh as u32)?;
    let mut lum = 0.0;
    for (px, c) in small.data_mut().chunks_exact_mut(4).zip(&img) {
        lum += 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        px.copy_from_slice(&[(c[0] * dim) as u8, (c[1] * dim) as u8, (c[2] * dim) as u8, 255]);
    }
    let lum = lum / (img.len().max(1) as f32 * 255.0);
    let mut full = Pixmap::new(w as u32, h as u32)?;
    let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() };
    let tf = Transform::from_scale(w as f32 / sw as f32, h as f32 / sh as f32);
    full.draw_pixmap(0, 0, small.as_ref(), &paint, tf, None);
    Some((full, lum))
}

fn box_blur(img: &mut [[f32; 3]], w: usize, h: usize, r: usize) {
    let mut tmp = img.to_vec();
    for pass in 0..2 {
        let (len, lines) = if pass == 0 { (w, h) } else { (h, w) };
        let idx = |line: usize, k: usize| if pass == 0 { line * w + k } else { k * w + line };
        for line in 0..lines {
            for k in 0..len {
                let lo = k.saturating_sub(r);
                let hi = (k + r).min(len - 1);
                let mut s = [0f32; 3];
                for j in lo..=hi {
                    let p = img[idx(line, j)];
                    s[0] += p[0];
                    s[1] += p[1];
                    s[2] += p[2];
                }
                let n = (hi - lo + 1) as f32;
                tmp[idx(line, k)] = [s[0] / n, s[1] / n, s[2] / n];
            }
        }
        img.copy_from_slice(&tmp);
    }
}
