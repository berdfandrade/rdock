//! A dock: layout com magnificação "olho de peixe", bounce ao abrir, indicadores de execução,
//! tooltips, menu de contexto, arrastar para reordenar / remover (com "puf") e ocultação automática.
//!
//! A lógica trabalha em coordenadas abstratas (u, v): `u` corre ao longo da dock e `v` é a
//! distância até a borda da tela. `uv_to_win` converte para pixels conforme a borda escolhida
//! (embaixo, esquerda ou direita); os ícones em si nunca giram.

use std::collections::HashMap;
use std::f32::consts::PI;
use std::time::{Duration, Instant};

use tiny_skia::{Color, IntRect, Pixmap, Transform};
use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

use crate::anim::{ease_out_cubic, Spring};
use crate::config::{self, Edge, HideMode, Settings};
use crate::draw;
use crate::menu::Menu;
use crate::{x, Action, Ctx, R};

const GAP: f32 = 8.0;
const HPAD: f32 = 6.0;
const PAD_TOP: f32 = 7.0;
const PAD_BOTTOM: f32 = 9.0;
const MARGIN: f32 = 6.0;
/// Espessura da janela: cabe o maior ícone ampliado, o bounce, a tooltip e o menu.
const THICK_BOTTOM: i32 = 400;
const THICK_SIDE: i32 = 540;
const SEP_W: f32 = 17.0;
/// Margem ao redor da prateleira no cache 9-slice (espaço para a sombra).
const SHELF_PAD: f32 = 20.0;
const BOUNCE_T: f32 = 0.62;
const BOUNCE_MAX: f32 = 12.0;
const REMOVE_DIST: f32 = 60.0;
const DRAG_THRESHOLD: f32 = 6.0;
const POOF_T: f32 = 0.45;
const TOOLTIP_MAX_W: f32 = 340.0;
const HIDE_DELAY: Duration = Duration::from_millis(600);
const LAUNCH_GRACE: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Launchpad,
    App { app: usize, pinned: bool },
    /// 0 = entre fixados e apps em execução; 1 = antes da Lixeira.
    Sep(u8),
    Trash,
}

struct Item {
    uid: u64,
    kind: Kind,
    scale: Spring,
    presence: Spring,
    dot: Spring,
    flip_u: Spring,
    flip_v: Spring,
    press: Spring,
    bounce: Option<Bounce>,
    launch_until: Option<Instant>,
    removing: bool,
}

#[derive(Clone, Copy)]
struct Bounce {
    t: f32,
    stop: bool,
}

struct Press {
    uid: u64,
    u: f32,
    v: f32,
}

struct Drag {
    uid: u64,
    u: f32,
    v: f32,
    off_u: f32,
    off_v: f32,
    remove: bool,
    /// Arraste vindo do Launchpad: `Some(true)` se o item foi criado para ele.
    external: Option<bool>,
}

/// Arraste XDND (ex.: um .desktop do gerenciador de arquivos) passando sobre a dock.
struct Dnd {
    source: Window,
    accept: bool,
    u: f32,
}

struct Poof {
    u: f32,
    v: f32,
    t: f32,
    key: String,
}

struct Tooltip {
    uid: Option<u64>,
    label: String,
    alpha: Spring,
    u: f32,
    v: f32,
}

type Box4 = (f32, f32, f32, f32);

fn union(a: Box4, b: Box4) -> Box4 {
    (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
}

pub struct Dock {
    pub win: Window,
    /// Criado junto com a janela (que é recriada ao mudar de borda).
    out: Option<x::Surface>,
    pix: Pixmap,
    /// Canto da janela na tela (para comparar com a janela ativa).
    origin: (i32, i32),
    /// Largura e altura da janela; comprimento do eixo `u`; espessura (eixo `v`).
    ww: f32,
    wh: f32,
    len: f32,
    thick: f32,
    /// Espaço já reservado por outros painéis em cada borda (embaixo, esquerda, direita).
    insets: (i32, i32, i32),
    s: Settings,
    items: Vec<Item>,
    next_uid: u64,
    /// Mouse em (u, v) e em pixels da janela (para o menu).
    mouse: Option<(f32, f32)>,
    mouse_px: (f32, f32),
    press: Option<Press>,
    drag: Option<Drag>,
    menu: Option<Menu>,
    tooltip: Tooltip,
    poofs: Vec<Poof>,
    last_ext: Option<Box4>,
    shape: (i32, i32, i32, i32),
    trash_full: bool,
    shelf: [Pixmap; 3],
    hide: Spring,
    left_at: Option<Instant>,
    dnd: Option<Dnd>,
    pub needs_frame: bool,
}

fn icon_key(apps: &[crate::apps::App], kind: Kind, trash_full: bool) -> &str {
    match kind {
        Kind::Launchpad => "@launchpad",
        Kind::Trash if trash_full => "user-trash-full",
        Kind::Trash => "user-trash",
        Kind::App { app, .. } => &apps[app].icon,
        Kind::Sep(_) => "",
    }
}

fn label(ctx: &Ctx, kind: Kind) -> String {
    match kind {
        Kind::Launchpad => "Launchpad".into(),
        Kind::Trash => "Lixeira".into(),
        Kind::App { app, .. } => ctx.apps[app].name.clone(),
        Kind::Sep(_) => String::new(),
    }
}

fn pos_of(items: &[Item], kind: Kind) -> usize {
    items.iter().position(|i| i.kind == kind).unwrap_or(items.len())
}

pub fn trash_is_full() -> bool {
    std::fs::read_dir(crate::apps::home().join(".local/share/Trash/files"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

fn shelf_t(icon: f32) -> f32 {
    icon + PAD_TOP + PAD_BOTTOM
}

fn radius(icon: f32) -> f32 {
    (shelf_t(icon) * 0.27).min(18.0)
}

/// Peças da prateleira: o tema PNG, se houver e carregar, senão o vidro embutido.
fn build_shelf(icon: f32, edge: Edge, theme: Option<&std::path::Path>) -> [Pixmap; 3] {
    theme.and_then(|p| build_theme_shelf(icon, edge, p)).unwrap_or_else(|| build_glass(icon, edge))
}

/// Tema PNG: a imagem (desenhada na horizontal) é escalada para a espessura da prateleira e
/// dividida em 3 faixas: as pontas (1/3 do lado menor cada) ficam fixas e o meio é repetido.
/// Nas bordas laterais as peças são giradas.
fn build_theme_shelf(icon: f32, edge: Edge, path: &std::path::Path) -> Option<[Pixmap; 3]> {
    let src = Pixmap::decode_png(&std::fs::read(path).ok()?).ok()?;
    let st = shelf_t(icon);
    let k = st / src.height() as f32;
    let (w, h) = (((src.width() as f32) * k).round().max(3.0), st);
    let mut img = Pixmap::new(w as u32, h as u32)?;
    let paint = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
    img.draw_pixmap(0, 0, src.as_ref(), &paint, Transform::from_scale(w / src.width() as f32, k), None);
    let b = ((src.width().min(src.height()) as f32 / 3.0) * k).round().clamp(1.0, (w - 1.0) / 2.0);
    let mid_w = (w - 2.0 * b).max(1.0);
    let pad = SHELF_PAD;
    // Cada peça leva a margem transparente SHELF_PAD, como o vidro (o desenho conta com ela).
    let piece = |sx: f32, cw: f32, pad_l: f32, pad_r: f32| -> Pixmap {
        let mut p = Pixmap::new((cw + pad_l + pad_r) as u32, (h + 2.0 * pad) as u32).unwrap();
        let part = img.clone_rect(IntRect::from_xywh(sx as i32, 0, cw as u32, h as u32).unwrap()).unwrap();
        draw::copy(&mut p, &part, pad_l as i32, pad as i32, cw as i32, h as i32);
        p
    };
    let (l, m, r) = (piece(0.0, b, pad, 0.0), piece(b, mid_w, 0.0, 0.0), piece(w - b, b, 0.0, pad));
    Some(match edge {
        Edge::Bottom => [l, m, r],
        // A base da imagem (lado da borda da tela) passa a apontar para a esquerda/direita.
        Edge::Left => [draw::rotate(&l, true), draw::rotate(&m, true), draw::rotate(&r, true)],
        Edge::Right => [draw::rotate(&r, false), draw::rotate(&m, false), draw::rotate(&l, false)],
    })
}

/// Renderiza a prateleira de vidro (sombra, gradiente, bordas) uma vez e corta em 3 partes:
/// esquerda/meio/direita na horizontal, topo/meio/base na vertical.
fn build_glass(icon: f32, edge: Edge) -> [Pixmap; 3] {
    let st = shelf_t(icon);
    let r = radius(icon);
    let cap = (r + SHELF_PAD + 2.0).ceil();
    let long = 2.0 * cap + 1.0;
    let horizontal = edge == Edge::Bottom;
    let (w, h) = if horizontal { (long, st + 2.0 * SHELF_PAD) } else { (st + 2.0 * SHELF_PAD, long) };
    let mut p = Pixmap::new(w as u32, h as u32).unwrap();
    let id = Transform::identity();
    let (x, y, sw, sh) = (SHELF_PAD, SHELF_PAD, w - 2.0 * SHELF_PAD, h - 2.0 * SHELF_PAD);
    draw::shadow(&mut p, x, y, sw, sh, r, 1.0, id);
    let paint = crate::icons::gradient_paint(Color::from_rgba8(78, 78, 88, 150), Color::from_rgba8(36, 36, 42, 170), y, y + sh);
    draw::fill(&mut p, &draw::rrect(x, y, sw, sh, r), &paint, id);
    draw::stroke(&mut p, &draw::rrect(x - 0.5, y - 0.5, sw + 1.0, sh + 1.0, r + 0.5), &draw::color(0, 0, 0, 0.35), 1.0, id);
    draw::stroke(&mut p, &draw::rrect(x + 0.5, y + 0.5, sw - 1.0, sh - 1.0, r - 0.5), &draw::color(255, 255, 255, 0.16), 1.0, id);
    let cut = |x: f32, y: f32, cw: f32, ch: f32| p.clone_rect(IntRect::from_xywh(x as i32, y as i32, cw as u32, ch as u32).unwrap()).unwrap();
    if horizontal {
        [cut(0.0, 0.0, cap, h), cut(cap, 0.0, 1.0, h), cut(cap + 1.0, 0.0, cap, h)]
    } else {
        [cut(0.0, 0.0, w, cap), cut(0.0, cap, w, 1.0), cut(0.0, cap + 1.0, w, cap)]
    }
}

impl Dock {
    pub fn new(ctx: &mut Ctx) -> R<Dock> {
        let (mx, my, mw, mh) = ctx.mon;
        let wa = x::prop32(&ctx.conn, ctx.root, ctx.atoms._NET_WORKAREA, AtomEnum::CARDINAL);
        let insets = if wa.len() >= 4 {
            let (wx, wy, ww, wh) = (wa[0] as i32, wa[1] as i32, wa[2] as i32, wa[3] as i32);
            (((my + mh) - (wy + wh)).clamp(0, 300), (wx - mx).clamp(0, 300), ((mx + mw) - (wx + ww)).clamp(0, 300))
        } else {
            (0, 0, 0)
        };
        let s = ctx.settings;
        let mut dock = Dock {
            win: x11rb::NONE,
            out: None,
            origin: (0, 0),
            pix: Pixmap::new(1, 1).ok_or("pixmap")?,
            ww: 0.0,
            wh: 0.0,
            len: 0.0,
            thick: 0.0,
            insets,
            s,
            items: Vec::new(),
            next_uid: 1,
            mouse: None,
            mouse_px: (0.0, 0.0),
            press: None,
            drag: None,
            menu: None,
            tooltip: Tooltip { uid: None, label: String::new(), alpha: Spring::new(0.0, 500.0, 1.0), u: 0.0, v: 0.0 },
            poofs: Vec::new(),
            last_ext: None,
            shape: (0, 0, 0, 0),
            trash_full: trash_is_full(),
            shelf: build_shelf(s.icon_size, s.edge, ctx.theme_path().as_deref()),
            hide: Spring::new(if s.hide == HideMode::Always { 1.0 } else { 0.0 }, 260.0, 1.0),
            left_at: None,
            dnd: None,
            needs_frame: true,
        };
        dock.create_window(ctx)?;
        let mut items = vec![dock.item(Kind::Launchpad, 1.0)];
        for id in config::load_pinned(&ctx.apps) {
            let found = ctx.apps.iter().position(|a| a.id == id);
            // Atalhos fixados a partir de um .desktop avulso ficam salvos pelo caminho completo.
            let found = found.or_else(|| id.starts_with('/').then(|| ctx.app_for_file(std::path::Path::new(&id))).flatten());
            if let Some(app) = found {
                items.push(dock.item(Kind::App { app, pinned: true }, 1.0));
            }
        }
        items.push(dock.item(Kind::Sep(0), 0.0));
        items.push(dock.item(Kind::Sep(1), 1.0));
        items.push(dock.item(Kind::Trash, 1.0));
        dock.items = items;
        dock.save(ctx);
        Ok(dock)
    }

    /// Cria (ou recria, ao mudar de borda) a janela da dock com as propriedades EWMH de painel.
    fn create_window(&mut self, ctx: &mut Ctx) -> R<()> {
        let conn = ctx.conn.clone();
        if let Some(mut out) = self.out.take() {
            out.free(&conn);
        }
        if self.win != x11rb::NONE {
            conn.destroy_window(self.win)?;
            ctx.own.retain(|w| *w != self.win);
        }
        let (mx, my, mw, mh) = ctx.mon;
        let (ib, il, ir) = self.insets;
        let geom = match self.s.edge {
            Edge::Bottom => (mx, my + mh - ib - THICK_BOTTOM, mw, THICK_BOTTOM),
            Edge::Left => (mx + il, my, THICK_SIDE, mh),
            Edge::Right => (mx + mw - ir - THICK_SIDE, my, THICK_SIDE, mh),
        };
        let events = EventMask::EXPOSURE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::ENTER_WINDOW
            | EventMask::LEAVE_WINDOW;
        let (win, out) = x::create_window(&conn, ctx.root, ctx.visual, ctx.colormap, geom, false, events)?;
        let a = &ctx.atoms;
        conn.change_property32(PropMode::REPLACE, win, a._NET_WM_WINDOW_TYPE, AtomEnum::ATOM, &[a._NET_WM_WINDOW_TYPE_DOCK])?;
        conn.change_property32(
            PropMode::REPLACE,
            win,
            a._NET_WM_STATE,
            AtomEnum::ATOM,
            &[a._NET_WM_STATE_STICKY, a._NET_WM_STATE_SKIP_TASKBAR, a._NET_WM_STATE_SKIP_PAGER, a._NET_WM_STATE_ABOVE],
        )?;
        conn.change_property32(PropMode::REPLACE, win, a._NET_WM_DESKTOP, AtomEnum::CARDINAL, &[u32::MAX])?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"rdock\0rdock\0")?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"rdock")?;
        conn.change_property8(PropMode::REPLACE, win, a._NET_WM_NAME, a.UTF8_STRING, b"rdock")?;
        // WM_HINTS: input = false (a dock nunca rouba o foco).
        conn.change_property32(PropMode::REPLACE, win, AtomEnum::WM_HINTS, AtomEnum::WM_HINTS, &[1, 0, 0, 0, 0, 0, 0, 0, 0])?;
        let mut hints = WmSizeHints::new();
        hints.position = Some((WmSizeHintsSpecification::UserSpecified, geom.0, geom.1));
        hints.size = Some((WmSizeHintsSpecification::UserSpecified, geom.2, geom.3));
        hints.min_size = Some((geom.2, geom.3));
        hints.max_size = Some((geom.2, geom.3));
        hints.set_normal_hints(&*conn, win)?;
        // Aceita arrastar-e-soltar XDND (versão 5): .desktop do gerenciador de arquivos.
        conn.change_property32(PropMode::REPLACE, win, a.XdndAware, AtomEnum::ATOM, &[5])?;
        conn.shape_rectangles(shape::SO::SET, shape::SK::INPUT, ClipOrdering::UNSORTED, win, 0, 0, &[])?;
        conn.map_window(win)?;
        ctx.own.push(win);

        self.win = win;
        self.out = Some(out);
        self.origin = (geom.0, geom.1);
        self.ww = geom.2 as f32;
        self.wh = geom.3 as f32;
        (self.len, self.thick) = if self.s.edge == Edge::Bottom { (self.ww, self.wh) } else { (self.wh, self.ww) };
        self.pix = Pixmap::new(geom.2 as u32, geom.3 as u32).ok_or("pixmap")?;
        self.shape = (0, 0, 0, 0);
        self.last_ext = None;
        self.set_strut(ctx)?;
        self.needs_frame = true;
        Ok(())
    }

    /// Reserva espaço na borda (janelas maximizadas não ficam por baixo). Zero se oculta automaticamente.
    fn set_strut(&self, ctx: &Ctx) -> R<()> {
        let (mx, my, mw, mh) = ctx.mon;
        let (ib, il, ir) = self.insets;
        let size = (MARGIN + shelf_t(self.s.icon_size)) as u32;
        let mut st = [0u32; 12];
        if self.s.hide == HideMode::Never {
            match self.s.edge {
                Edge::Bottom => (st[3], st[10], st[11]) = (ib as u32 + size, mx as u32, (mx + mw - 1) as u32),
                Edge::Left => (st[0], st[4], st[5]) = (il as u32 + size, my as u32, (my + mh - 1) as u32),
                Edge::Right => (st[1], st[6], st[7]) = (ir as u32 + size, my as u32, (my + mh - 1) as u32),
            }
        }
        let a = &ctx.atoms;
        ctx.conn.change_property32(PropMode::REPLACE, self.win, a._NET_WM_STRUT, AtomEnum::CARDINAL, &st[..4])?;
        ctx.conn.change_property32(PropMode::REPLACE, self.win, a._NET_WM_STRUT_PARTIAL, AtomEnum::CARDINAL, &st)?;
        Ok(())
    }

    /// Aplica `ctx.settings` (chamado quando o usuário muda uma preferência).
    pub fn apply(&mut self, ctx: &mut Ctx) -> R<()> {
        let (old, s) = (self.s, ctx.settings);
        if old == s {
            return Ok(());
        }
        self.s = s;
        if s.icon_size != old.icon_size || s.edge != old.edge {
            self.shelf = build_shelf(s.icon_size, s.edge, ctx.theme_path().as_deref());
        }
        if !s.bounce {
            for it in &mut self.items {
                it.bounce = None;
            }
        }
        if s.edge != old.edge {
            if self.menu.is_some() {
                self.close_menu(ctx);
            }
            (self.press, self.drag, self.mouse) = (None, None, None);
            self.create_window(ctx)?;
        } else if s.icon_size != old.icon_size || s.hide != old.hide {
            self.set_strut(ctx)?;
        }
        self.damage();
        Ok(())
    }

    /// Recarrega o tema da prateleira (ao trocar de tema).
    pub fn reload_theme(&mut self, ctx: &Ctx) {
        self.shelf = build_shelf(self.s.icon_size, self.s.edge, ctx.theme_path().as_deref());
        self.damage();
    }

    /// Nomes de ícones que a dock exibe (carregados primeiro).
    pub fn icon_names(&self, ctx: &Ctx) -> Vec<String> {
        let mut v: Vec<String> = self.items.iter().map(|i| icon_key(&ctx.apps, i.kind, false).to_string()).collect();
        v.push("user-trash-full".into());
        v.retain(|s| !s.is_empty());
        v
    }

    fn item(&mut self, kind: Kind, presence: f32) -> Item {
        self.next_uid += 1;
        let mut p = Spring::new(presence, 260.0, 1.0);
        p.target = 1.0;
        Item {
            uid: self.next_uid,
            kind,
            scale: Spring::new(1.0, 900.0, 1.0),
            presence: p,
            dot: Spring::new(0.0, 160.0, 1.0),
            flip_u: Spring::new(0.0, 320.0, 0.85),
            flip_v: Spring::new(0.0, 320.0, 0.85),
            press: Spring::new(0.0, 900.0, 1.0),
            bounce: None,
            launch_until: None,
            removing: false,
        }
    }

    // ---------------------------------------------------------------- geometria

    fn icon(&self) -> f32 {
        self.s.icon_size
    }

    /// Distância da borda até a base dos ícones.
    fn base(&self) -> f32 {
        MARGIN + PAD_BOTTOM
    }

    fn shelf_far(&self) -> f32 {
        MARGIN + shelf_t(self.icon())
    }

    fn hide_off(&self) -> f32 {
        self.hide.value * (self.shelf_far() + SHELF_PAD)
    }

    /// (u, v) → pixels da janela, já deslocado pela animação de ocultar.
    fn uv_to_win(&self, u: f32, v: f32) -> (f32, f32) {
        self.to_win_raw(u, v - self.hide_off())
    }

    fn to_win_raw(&self, u: f32, v: f32) -> (f32, f32) {
        match self.s.edge {
            Edge::Bottom => (u, self.wh - v),
            Edge::Left => (v, u),
            Edge::Right => (self.ww - v, u),
        }
    }

    fn win_to_uv(&self, x: f32, y: f32) -> (f32, f32) {
        let (u, v) = match self.s.edge {
            Edge::Bottom => (x, self.wh - y),
            Edge::Left => (y, x),
            Edge::Right => (y, self.ww - x),
        };
        (u, v + self.hide_off())
    }

    /// Retângulo (u0..u1, v0..v1) em pixels da janela: (x0, y0, x1, y1).
    fn rect_win(&self, u0: f32, u1: f32, v0: f32, v1: f32, raw: bool) -> Box4 {
        let f = |u, v| if raw { self.to_win_raw(u, v) } else { self.uv_to_win(u, v) };
        let (a, b) = (f(u0, v0), f(u1, v1));
        (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
    }

    /// Posição (canto superior esquerdo) de uma caixa w×h encostada "além" do ponto (u, v).
    fn place_box(&self, u: f32, v: f32, w: f32, h: f32) -> (f32, f32) {
        let (ax, ay) = self.uv_to_win(u, v);
        match self.s.edge {
            Edge::Bottom => (ax - w / 2.0, ay - h),
            Edge::Left => (ax, ay - h / 2.0),
            Edge::Right => (ax - w, ay - h / 2.0),
        }
    }

    /// A área da dock (visível, sem ampliação) cruza o retângulo da janela ativa?
    fn overlaps(&self, rect: Option<(i32, i32, i32, i32)>) -> bool {
        let Some((rx, ry, rw, rh)) = rect else { return false };
        let (_, u0, u1) = self.layout(false);
        let b = self.rect_win(u0, u1, 0.0, self.shelf_far() + 2.0, true);
        let (ox, oy) = (self.origin.0 as f32, self.origin.1 as f32);
        let (x0, y0, x1, y1) = (b.0 + ox, b.1 + oy, b.2 + ox, b.3 + oy);
        let (rx0, ry0, rx1, ry1) = (rx as f32, ry as f32, (rx + rw) as f32, (ry + rh) as f32);
        x0 < rx1 && rx0 < x1 && y0 < ry1 && ry0 < y1
    }

    fn mag(&self, d: f32) -> f32 {
        let t = (d.abs() / (self.icon() * 3.1)).min(1.0);
        1.0 + (self.s.max_scale() - 1.0) * (0.5 + 0.5 * (PI * t).cos())
    }

    fn index_of(&self, uid: u64) -> Option<usize> {
        self.items.iter().position(|i| i.uid == uid)
    }

    fn find_app(&self, app: usize) -> Option<usize> {
        self.items.iter().position(|i| matches!(i.kind, Kind::App { app: a, .. } if a == app) && !i.removing)
    }

    fn slot_w(&self, it: &Item, scaled: bool) -> f32 {
        let p = it.presence.value.max(0.0);
        match it.kind {
            Kind::Sep(_) => SEP_W * p,
            _ => (self.icon() * if scaled { it.scale.value } else { 1.0 } + GAP) * p,
        }
    }

    /// Centros dos itens (em u) e extremidades da prateleira.
    fn layout(&self, scaled: bool) -> (Vec<f32>, f32, f32) {
        let ws: Vec<f32> = self.items.iter().map(|i| self.slot_w(i, scaled)).collect();
        let total: f32 = ws.iter().sum::<f32>() + 2.0 * HPAD;
        let u0 = ((self.len - total) / 2.0).round();
        let mut u = u0 + HPAD;
        let centers = ws
            .iter()
            .map(|w| {
                let c = u + w / 2.0;
                u += w;
                c
            })
            .collect();
        (centers, u0, u0 + total)
    }

    fn hit(&self, u: f32, v: f32) -> Option<usize> {
        let (cs, _, _) = self.layout(true);
        self.items.iter().enumerate().position(|(i, it)| {
            if matches!(it.kind, Kind::Sep(_)) || it.removing {
                return false;
            }
            let far = self.base() + self.icon() * it.scale.value + 4.0;
            (u - cs[i]).abs() <= self.slot_w(it, true) / 2.0 && v <= far
        })
    }

    fn on_shelf(&self, u: f32, v: f32) -> bool {
        let (_, u0, u1) = self.layout(true);
        u >= u0 && u <= u1 && v <= self.shelf_far() + 2.0
    }

    /// Aplica uma mudança estrutural animando os itens das posições antigas para as novas (FLIP).
    fn with_flip(&mut self, f: impl FnOnce(&mut Vec<Item>)) {
        let (cs, _, _) = self.layout(true);
        let before: HashMap<u64, f32> = self.items.iter().zip(cs).map(|(it, c)| (it.uid, c + it.flip_u.value)).collect();
        f(&mut self.items);
        let (cs, _, _) = self.layout(true);
        for (it, c) in self.items.iter_mut().zip(cs) {
            if let Some(b) = before.get(&it.uid) {
                it.flip_u.value = b - c;
            }
        }
        self.needs_frame = true;
    }

    fn save(&self, ctx: &mut Ctx) {
        ctx.pinned.clear();
        let mut ids = Vec::new();
        for it in &self.items {
            if let Kind::App { app, pinned: true } = it.kind {
                if !it.removing {
                    ctx.pinned.insert(app);
                    ids.push(ctx.apps[app].id.clone());
                }
            }
        }
        config::save_pinned(&ids);
    }

    pub fn set_trash_full(&mut self, full: bool) {
        if full != self.trash_full {
            self.trash_full = full;
            self.needs_frame = true;
        }
    }

    pub fn damage(&mut self) {
        self.last_ext = Some((0.0, 0.0, self.ww, self.wh));
        self.needs_frame = true;
    }

    // ---------------------------------------------------------------- estado das janelas

    pub fn sync_running(&mut self, ctx: &Ctx) {
        for it in &mut self.items {
            if let Kind::App { app, .. } = it.kind {
                let running = ctx.running.contains_key(&app);
                it.dot.target = if running { 1.0 } else { 0.0 };
                if running {
                    if let Some(b) = &mut it.bounce {
                        b.stop = true;
                    }
                    it.launch_until = None;
                }
            }
        }
        let mut new: Vec<usize> = ctx.running.keys().copied().filter(|&a| self.find_app(a).is_none()).collect();
        new.sort_unstable();
        for app in new {
            let mut it = self.item(Kind::App { app, pinned: false }, 0.0);
            it.dot.target = 1.0;
            let at = pos_of(&self.items, Kind::Sep(1));
            self.items.insert(at, it);
        }
        self.needs_frame = true;
    }

    pub fn launch(&mut self, ctx: &mut Ctx, app: usize) {
        if let Some(wins) = ctx.running.get(&app) {
            if let Some(&w) = wins.last() {
                x::activate(&ctx.conn, &ctx.atoms, ctx.root, w, ctx.time);
            }
            return;
        }
        if !ctx.apps[app].launchable() {
            return;
        }
        crate::apps::launch(&ctx.apps[app]);
        let i = match self.find_app(app) {
            Some(i) => i,
            None => {
                let it = self.item(Kind::App { app, pinned: false }, 0.0);
                let at = pos_of(&self.items, Kind::Sep(1));
                self.items.insert(at, it);
                at
            }
        };
        let bounce = self.s.bounce;
        let it = &mut self.items[i];
        if bounce {
            it.bounce = Some(Bounce { t: 0.0, stop: false });
        }
        it.launch_until = Some(Instant::now() + LAUNCH_GRACE);
        self.needs_frame = true;
    }

    pub fn toggle_pin(&mut self, ctx: &mut Ctx, app: usize) {
        if !ctx.apps[app].launchable() {
            return;
        }
        let running = ctx.running.contains_key(&app);
        match self.find_app(app) {
            Some(i) => {
                let pinned = matches!(self.items[i].kind, Kind::App { pinned: true, .. });
                if pinned && !running {
                    self.items[i].removing = true;
                    self.items[i].presence.target = 0.0;
                } else {
                    self.with_flip(|items| {
                        let mut it = items.remove(i);
                        it.kind = Kind::App { app, pinned: !pinned };
                        let at = pos_of(items, Kind::Sep(if pinned { 1 } else { 0 }));
                        items.insert(at, it);
                    });
                }
            }
            None => {
                let it = self.item(Kind::App { app, pinned: true }, 0.0);
                let at = pos_of(&self.items, Kind::Sep(0));
                self.items.insert(at, it);
            }
        }
        self.save(ctx);
        self.needs_frame = true;
    }

    // ---------------------------------------------------------------- entrada

    pub fn on_motion(&mut self, ctx: &mut Ctx, x: f32, y: f32) {
        let (u, v) = self.win_to_uv(x, y);
        self.mouse = Some((u, v));
        self.mouse_px = (x, y);
        self.needs_frame = true;
        if let Some(m) = &mut self.menu {
            m.hovered = m.hit(x, y);
            return;
        }
        if self.drag.is_none() {
            if let Some(p) = &self.press {
                if (u - p.u).hypot(v - p.v) > DRAG_THRESHOLD {
                    self.start_drag(ctx);
                }
            }
        }
        if self.drag.is_some() {
            self.update_drag(u, v);
        }
    }

    pub fn on_leave(&mut self) {
        if self.press.is_none() && self.drag.is_none() && self.menu.is_none() {
            self.mouse = None;
            self.needs_frame = true;
        }
    }

    pub fn on_press(&mut self, ctx: &mut Ctx, button: u8, x: f32, y: f32) {
        self.needs_frame = true;
        if self.menu.is_some() {
            if self.menu.as_ref().and_then(|m| m.hit(x, y)).is_none() {
                self.close_menu(ctx);
            }
            return;
        }
        let (u, v) = self.win_to_uv(x, y);
        let hit = self.hit(u, v);
        match (button, hit) {
            (1, Some(i)) => {
                self.items[i].press.target = 1.0;
                self.press = Some(Press { uid: self.items[i].uid, u, v });
            }
            (3, Some(i)) if matches!(self.items[i].kind, Kind::App { .. }) => self.open_app_menu(ctx, i),
            (3, _) if hit.is_some() || self.on_shelf(u, v) => self.open_dock_menu(ctx, u),
            _ => {}
        }
    }

    pub fn on_release(&mut self, ctx: &mut Ctx, button: u8, x: f32, y: f32) -> Option<Action> {
        self.needs_frame = true;
        if let Some(m) = &self.menu {
            let act = m.hit(x, y).map(|k| m.items[k].1);
            if act.is_some() {
                self.close_menu(ctx);
            }
            return act;
        }
        if button != 1 {
            return None;
        }
        if self.drag.is_some() {
            self.finish_drag(ctx);
            return None;
        }
        let p = self.press.take()?;
        let i = self.index_of(p.uid)?;
        self.items[i].press.target = 0.0;
        let (u, v) = self.win_to_uv(x, y);
        if self.hit(u, v) != Some(i) {
            return None;
        }
        match self.items[i].kind {
            Kind::Launchpad => Some(Action::ToggleLaunchpad),
            Kind::Trash => Some(Action::OpenTrash),
            Kind::App { app, .. } => Some(Action::Launch(app)),
            Kind::Sep(_) => None,
        }
    }

    fn open_app_menu(&mut self, ctx: &mut Ctx, i: usize) {
        let Kind::App { app, pinned } = self.items[i].kind else {
            return;
        };
        let running = ctx.running.contains_key(&app);
        let mut items = vec![(if running { "Mostrar" } else { "Abrir" }.to_string(), Action::Launch(app))];
        if ctx.apps[app].launchable() {
            let l = if pinned { "Remover do Dock" } else { "Manter no Dock" };
            items.push((l.to_string(), Action::TogglePin(app)));
        }
        if running {
            items.push(("Encerrar".to_string(), Action::Quit(app)));
        }
        let (cs, _, _) = self.layout(true);
        let far = self.base() + self.icon() * self.items[i].scale.value;
        self.show_menu(ctx, items, cs[i], far + 10.0);
    }

    /// Menu da própria dock (clique direito no separador, na prateleira, no Launchpad ou na Lixeira).
    fn open_dock_menu(&mut self, ctx: &mut Ctx, u: f32) {
        let s = self.s;
        let mut items = vec![
            (if s.magnification { "Desativar ampliação" } else { "Ativar ampliação" }.to_string(), Action::ToggleMagnification),
        ];
        for (mode, l) in [(HideMode::Never, "Nunca ocultar"), (HideMode::Always, "Ocultar sempre"), (HideMode::Smart, "Ocultação inteligente")] {
            if mode != s.hide {
                items.insert(items.len() - 1, (l.to_string(), Action::SetHide(mode)));
            }
        }
        for (edge, l) in [(Edge::Left, "Posicionar à esquerda"), (Edge::Bottom, "Posicionar embaixo"), (Edge::Right, "Posicionar à direita")] {
            if edge != s.edge {
                items.push((l.to_string(), Action::SetEdge(edge)));
            }
        }
        items.push(("Preferências do Dock…".to_string(), Action::OpenSettings));
        let far = self.shelf_far() + 4.0;
        self.show_menu(ctx, items, u, far);
    }

    fn show_menu(&mut self, ctx: &mut Ctx, items: Vec<(String, Action)>, u: f32, v: f32) {
        let mut m = Menu::new(items, &ctx.text);
        let (w, h) = m.size();
        let (x, y) = self.place_box(u, v, w, h);
        m.place(x, y, self.uv_to_win(u, v), (self.ww, self.wh));
        self.menu = Some(m);
        let _ = ctx.conn.grab_pointer(
            false,
            self.win,
            EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        );
    }

    fn close_menu(&mut self, ctx: &mut Ctx) {
        self.menu = None;
        self.mouse = None;
        let _ = ctx.conn.ungrab_pointer(x11rb::CURRENT_TIME);
    }

    fn start_drag(&mut self, ctx: &Ctx) {
        let Some(p) = self.press.take() else { return };
        let Some(i) = self.index_of(p.uid) else { return };
        self.items[i].press.target = 0.0;
        let Kind::App { app, .. } = self.items[i].kind else { return };
        if !ctx.apps[app].launchable() {
            return;
        }
        let (cs, _, _) = self.layout(true);
        let size = self.icon() * self.items[i].scale.value;
        self.drag = Some(Drag {
            uid: p.uid,
            u: p.u,
            v: p.v,
            off_u: cs[i] - p.u,
            off_v: self.base() + size / 2.0 - p.v,
            remove: false,
            external: None,
        });
    }

    /// Começa a arrastar `app` vindo de fora da dock (Launchpad), com o ponteiro em (rx, ry) na tela.
    /// A dock agarra o ponteiro até o botão ser solto; soltar longe da dock cancela.
    pub fn begin_external_drag(&mut self, ctx: &mut Ctx, app: usize, rx: f32, ry: f32) {
        if !ctx.apps[app].launchable() {
            return;
        }
        if self.menu.is_some() {
            self.close_menu(ctx);
        }
        let (u, v) = self.win_to_uv(rx - self.origin.0 as f32, ry - self.origin.1 as f32);
        let (uid, is_new) = match self.find_app(app) {
            Some(i) => (self.items[i].uid, false),
            None => {
                let mut it = self.item(Kind::App { app, pinned: true }, 0.0);
                it.presence.target = 0.0;
                let uid = it.uid;
                let at = pos_of(&self.items, Kind::Sep(0));
                self.items.insert(at, it);
                (uid, true)
            }
        };
        self.press = None;
        self.drag = Some(Drag { uid, u, v, off_u: 0.0, off_v: 0.0, remove: true, external: Some(is_new) });
        self.mouse = Some((u, v));
        let _ = ctx.conn.grab_pointer(
            false,
            self.win,
            EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        );
        self.update_drag(u, v);
        self.needs_frame = true;
    }

    // ---------------------------------------------------------------- XDND (soltar arquivos)

    fn xdnd_send(&self, ctx: &Ctx, source: Window, ty: Atom, data: [u32; 5]) {
        let ev = ClientMessageEvent::new(32, source, ty, data);
        let _ = ctx.conn.send_event(false, source, EventMask::NO_EVENT, ev);
    }

    /// Trata as mensagens XdndEnter/Position/Leave/Drop enviadas pela origem do arraste.
    pub fn on_xdnd(&mut self, ctx: &mut Ctx, e: &ClientMessageEvent) {
        let a = ctx.atoms;
        let d = e.data.as_data32();
        if e.type_ == a.XdndEnter {
            let types: Vec<u32> = if d[1] & 1 != 0 {
                x::prop32(&ctx.conn, d[0], a.XdndTypeList, AtomEnum::ATOM)
            } else {
                d[2..5].to_vec()
            };
            self.dnd = Some(Dnd { source: d[0], accept: types.contains(&a.TEXT_URI_LIST), u: 0.0 });
        } else if e.type_ == a.XdndPosition {
            let Some(dnd) = &self.dnd else { return };
            let (rx, ry) = ((d[2] >> 16) as f32, (d[2] & 0xffff) as f32);
            let (u, v) = self.win_to_uv(rx - self.origin.0 as f32, ry - self.origin.1 as f32);
            let (source, accept) = (dnd.source, dnd.accept);
            if let Some(dnd) = &mut self.dnd {
                dnd.u = u;
            }
            // Os ícones se ampliam sob o arquivo arrastado, como sob o mouse.
            self.mouse = Some((u, v));
            self.needs_frame = true;
            let flags = if accept { 1 | 2 } else { 2 };
            self.xdnd_send(ctx, source, a.XdndStatus, [self.win, flags, 0, 0, if accept { a.XdndActionCopy } else { 0 }]);
        } else if e.type_ == a.XdndLeave {
            self.dnd = None;
            self.mouse = None;
            self.needs_frame = true;
        } else if e.type_ == a.XdndDrop {
            let Some(dnd) = &self.dnd else { return };
            if dnd.accept {
                let _ = ctx.conn.convert_selection(self.win, a.XdndSelection, a.TEXT_URI_LIST, a._RDOCK_DND, d[2]);
            } else {
                self.xdnd_send(ctx, dnd.source, a.XdndFinished, [self.win, 0, 0, 0, 0]);
                self.dnd = None;
            }
        }
    }

    /// Chegou a lista de URIs soltas: fixa cada .desktop na posição do soltar.
    pub fn on_xdnd_data(&mut self, ctx: &mut Ctx, property: Atom) {
        let Some(dnd) = self.dnd.take() else { return };
        let a = ctx.atoms;
        let mut ok = false;
        if property != x11rb::NONE {
            let data = ctx
                .conn
                .get_property(true, self.win, property, AtomEnum::ANY, 0, 1 << 20)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.value)
                .unwrap_or_default();
            for uri in String::from_utf8_lossy(&data).lines() {
                let Some(path) = crate::apps::uri_to_path(uri.trim()) else { continue };
                if path.extension().is_some_and(|e| e == "desktop") {
                    if let Some(app) = ctx.app_for_file(&path) {
                        self.pin_at(ctx, app, dnd.u);
                        ok = true;
                    }
                }
            }
        }
        self.xdnd_send(ctx, dnd.source, a.XdndFinished, [self.win, ok as u32, if ok { a.XdndActionCopy } else { 0 }, 0, 0]);
        self.mouse = None;
        self.needs_frame = true;
    }

    /// Fixa `app` na posição `u` da dock (soltar um .desktop vindo de fora).
    pub fn pin_at(&mut self, ctx: &mut Ctx, app: usize, u: f32) {
        if !ctx.apps[app].launchable() {
            return;
        }
        let existing = self.find_app(app);
        let (cs, _, _) = self.layout(true);
        let sep0 = pos_of(&self.items, Kind::Sep(0));
        let target = 1 + (1..sep0).filter(|&j| Some(j) != existing && cs[j] < u).count();
        match existing {
            Some(i) => self.with_flip(|items| {
                let mut it = items.remove(i);
                it.kind = Kind::App { app, pinned: true };
                items.insert(target.min(items.len()), it);
            }),
            None => {
                let it = self.item(Kind::App { app, pinned: true }, 0.0);
                self.items.insert(target, it);
            }
        }
        self.save(ctx);
        self.needs_frame = true;
    }

    fn update_drag(&mut self, u: f32, v: f32) {
        let Some(uid) = self.drag.as_ref().map(|d| d.uid) else { return };
        let Some(i) = self.index_of(uid) else { return };
        let pinned = matches!(self.items[i].kind, Kind::App { pinned: true, .. });
        let remove = pinned && v > self.shelf_far() + REMOVE_DIST;
        if let Some(d) = &mut self.drag {
            (d.u, d.v, d.remove) = (u, v, remove);
        }
        self.items[i].presence.target = if remove { 0.0 } else { 1.0 };
        if remove {
            return;
        }
        let sep0 = pos_of(&self.items, Kind::Sep(0));
        let (cs, _, _) = self.layout(true);
        if !pinned && u >= cs[sep0] {
            return;
        }
        let target = 1 + (1..sep0).filter(|&j| j != i && cs[j] < u).count();
        if pinned && target == i {
            return;
        }
        let Kind::App { app, .. } = self.items[i].kind else { return };
        self.with_flip(|items| {
            let mut it = items.remove(i);
            it.kind = Kind::App { app, pinned: true };
            items.insert(target, it);
        });
    }

    /// Centro (u, v) do ícone arrastado, limitado à janela.
    fn drag_center(&self, d: &Drag, size: f32) -> (f32, f32) {
        (d.u + d.off_u, (d.v + d.off_v).min(self.thick - size / 2.0 - 2.0))
    }

    fn finish_drag(&mut self, ctx: &mut Ctx) {
        let Some(d) = self.drag.take() else { return };
        let Some(i) = self.index_of(d.uid) else { return };
        let Kind::App { app, .. } = self.items[i].kind else { return };
        let size = self.icon() * self.items[i].scale.value;
        let (cu, cv) = self.drag_center(&d, size);
        if d.external.is_some() {
            let _ = ctx.conn.ungrab_pointer(x11rb::CURRENT_TIME);
            self.mouse = None;
        }
        if d.remove && d.external == Some(true) {
            // Veio do Launchpad e foi solto longe da dock: cancela sem "puf".
            self.items[i].removing = true;
            self.items[i].presence.snap(0.0);
        } else if d.remove && d.external == Some(false) {
            self.items[i].presence.target = 1.0;
        } else if d.remove {
            if ctx.running.contains_key(&app) {
                self.with_flip(|items| {
                    let mut it = items.remove(i);
                    it.kind = Kind::App { app, pinned: false };
                    it.presence.target = 1.0;
                    let at = pos_of(items, Kind::Sep(1));
                    items.insert(at, it);
                });
            } else {
                self.items[i].removing = true;
                self.items[i].presence.snap(0.0);
                self.poofs.push(Poof { u: cu, v: cv, t: 0.0, key: ctx.apps[app].icon.clone() });
            }
        } else {
            let (cs, _, _) = self.layout(true);
            let base = self.base();
            let it = &mut self.items[i];
            it.flip_u.value = cu - cs[i];
            it.flip_v.value = cv - (base + size / 2.0);
        }
        self.save(ctx);
    }

    // ---------------------------------------------------------------- animação

    pub fn tick(&mut self, ctx: &mut Ctx, dt: f32) -> bool {
        let now = Instant::now();
        let mut anim = false;
        let drag_uid = self.drag.as_ref().map(|d| d.uid);
        for it in &mut self.items {
            if let Kind::App { app, pinned: false } = it.kind {
                let launching = it.bounce.is_some() || it.launch_until.is_some_and(|d| now < d);
                if !ctx.running.contains_key(&app) && !launching && drag_uid != Some(it.uid) && !it.removing {
                    it.removing = true;
                    it.presence.target = 0.0;
                }
            }
        }

        // Ocultação: "sempre" some sem o mouse; "inteligente" só quando a janela ativa encosta na dock.
        // Nos dois casos ela volta quando o mouse toca a borda.
        let engaged = self.mouse.is_some() || self.menu.is_some() || self.drag.is_some() || self.press.is_some();
        let wants_hide = match self.s.hide {
            HideMode::Never => false,
            HideMode::Always => true,
            HideMode::Smart => self.overlaps(ctx.active_rect),
        };
        if !wants_hide || engaged {
            self.hide.target = 0.0;
            self.left_at = None;
        } else {
            let since = *self.left_at.get_or_insert(now);
            if now - since >= HIDE_DELAY {
                self.hide.target = 1.0;
            } else {
                anim = true;
            }
        }
        anim |= self.hide.step(dt);

        let has_unpinned = self.items.iter().any(|i| matches!(i.kind, Kind::App { pinned: false, .. }) && !i.removing);
        let remove_mode = self.drag.as_ref().is_some_and(|d| d.remove);
        let hover_u = match (self.mouse, &self.menu) {
            (Some((u, _)), None) if !remove_mode => Some(u),
            _ => None,
        };
        let (base, _, _) = self.layout(false);
        let targets: Vec<f32> = base.iter().map(|&b| hover_u.map_or(1.0, |mu| self.mag(mu - b))).collect();
        for (it, target) in self.items.iter_mut().zip(targets) {
            if let Kind::Sep(0) = it.kind {
                it.presence.target = if has_unpinned { 1.0 } else { 0.0 };
            }
            it.scale.target = target;
            anim |= it.scale.step(dt);
            anim |= it.presence.step(dt);
            anim |= it.dot.step(dt);
            anim |= it.flip_u.step(dt);
            anim |= it.flip_v.step(dt);
            anim |= it.press.step(dt);
            if let Some(b) = &mut it.bounce {
                anim = true;
                let c0 = (b.t / BOUNCE_T).floor();
                b.t += dt;
                if (b.t / BOUNCE_T).floor() > c0 && (b.stop || b.t > BOUNCE_MAX) {
                    it.bounce = None;
                }
            }
        }
        let n = self.items.len();
        self.items.retain(|it| !(it.removing && it.presence.target == 0.0 && it.presence.value < 0.005));
        anim |= n != self.items.len();

        let hovered = if self.drag.is_none() && self.menu.is_none() {
            self.mouse.and_then(|(u, v)| self.hit(u, v))
        } else {
            None
        };
        match hovered {
            Some(i) => {
                self.tooltip.uid = Some(self.items[i].uid);
                self.tooltip.label = ctx.text.ellipsize(&label(ctx, self.items[i].kind), 15.5, TOOLTIP_MAX_W);
                self.tooltip.alpha.target = 1.0;
            }
            None => self.tooltip.alpha.target = 0.0,
        }
        anim |= self.tooltip.alpha.step(dt);
        for p in &mut self.poofs {
            p.t += dt;
        }
        self.poofs.retain(|p| p.t < POOF_T);
        anim |= !self.poofs.is_empty();
        if let Some(m) = &mut self.menu {
            anim |= m.anim.step(dt);
        }
        anim
    }

    // ---------------------------------------------------------------- desenho

    pub fn render(&mut self, ctx: &mut Ctx) -> R<()> {
        // Tudo o que foi desenhado no quadro anterior está dentro de `last_ext`: basta limpar ali.
        let full = (0.0, 0.0, self.ww, self.wh);
        let last = self.last_ext.unwrap_or(full);
        draw::clear_rect(&mut self.pix, last.0.floor() as i32 - 1, last.1.floor() as i32 - 1, last.2.ceil() as i32 + 1, last.3.ceil() as i32 + 1);

        let icon = self.icon();
        let (cs, u0, u1) = self.layout(true);
        let (far, base) = (self.shelf_far(), self.base());
        let shelf = self.rect_win(u0, u1, MARGIN, far, false);
        let mut ext = (shelf.0 - SHELF_PAD - 4.0, shelf.1 - SHELF_PAD - 4.0, shelf.2 + SHELF_PAD + 4.0, shelf.3 + SHELF_PAD + 4.0);

        // Prateleira de vidro (9-slice pré-renderizado, copiado direto na memória)
        if u1 - u0 > 2.0 * radius(icon) {
            let [a, mid, b] = &self.shelf;
            let (sx, sy) = ((shelf.0 - SHELF_PAD).round() as i32, (shelf.1 - SHELF_PAD).round() as i32);
            let (aw, ah) = (a.width() as i32, a.height() as i32);
            let (bw, bh) = (b.width() as i32, b.height() as i32);
            if self.s.edge == Edge::Bottom {
                let inner = ((shelf.2 - shelf.0).round() as i32 + 2 * SHELF_PAD as i32 - aw - bw).max(0);
                draw::copy(&mut self.pix, a, sx, sy, aw, ah);
                draw::copy(&mut self.pix, mid, sx + aw, sy, inner, mid.height() as i32);
                draw::copy(&mut self.pix, b, sx + aw + inner, sy, bw, bh);
            } else {
                let inner = ((shelf.3 - shelf.1).round() as i32 + 2 * SHELF_PAD as i32 - ah - bh).max(0);
                draw::copy(&mut self.pix, a, sx, sy, aw, ah);
                draw::copy(&mut self.pix, mid, sx, sy + ah, mid.width() as i32, inner);
                draw::copy(&mut self.pix, b, sx, sy + ah + inner, bw, bh);
            }
        }

        let drag_uid = self.drag.as_ref().map(|d| d.uid);
        for (i, it) in self.items.iter().enumerate() {
            let p = it.presence.value.clamp(0.0, 1.0);
            let cu = cs[i] + it.flip_u.value;
            if let Kind::Sep(_) = it.kind {
                if p > 0.01 {
                    let r = self.rect_win(cu - 0.5, cu + 0.5, MARGIN + 11.0, far - 11.0, false);
                    if let Some(r) = tiny_skia::Rect::from_ltrb(r.0, r.1, r.2.max(r.0 + 1.0), r.3.max(r.1 + 1.0)) {
                        self.pix.fill_rect(r, &draw::color(255, 255, 255, 0.28 * p), Transform::identity(), None);
                    }
                }
                continue;
            }
            if drag_uid == Some(it.uid) || it.removing && it.presence.target == 0.0 && drag_uid.is_none() && p < 0.02 {
                continue;
            }
            let size = icon * it.scale.value * (0.35 + 0.65 * p);
            let bounce = it.bounce.map_or(0.0, |b| {
                let ph = (b.t % BOUNCE_T) / BOUNCE_T;
                icon * 0.62 * 4.0 * ph * (1.0 - ph)
            });
            let edge_v = base + it.flip_v.value + bounce;
            let (px, py) = self.uv_to_win(cu, edge_v + size / 2.0);
            let key = icon_key(&ctx.apps, it.kind, self.trash_full);
            draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, key, px, py - size / 2.0, size, p, it.press.value * 0.8);
            ext = union(ext, (px - size / 2.0, py - size / 2.0, px + size / 2.0, py + size / 2.0));
            if self.s.indicators && it.dot.value > 0.01 {
                let (dx, dy) = self.uv_to_win(cu, MARGIN + 4.0);
                draw::circle(&mut self.pix, dx, dy, 2.2, &draw::color(255, 255, 255, 0.9 * it.dot.value * p));
            }
            if self.tooltip.uid == Some(it.uid) {
                (self.tooltip.u, self.tooltip.v) = (cu, edge_v + size + 10.0);
            }
        }

        // Ícone sendo arrastado
        if let Some(d) = &self.drag {
            if let Some(i) = self.index_of(d.uid) {
                let it = &self.items[i];
                let size = icon * it.scale.value.max(1.0);
                let (cu, cv) = self.drag_center(d, size);
                let (px, py) = self.uv_to_win(cu, cv);
                let key = icon_key(&ctx.apps, it.kind, self.trash_full);
                let op = if d.remove { 0.55 } else { 1.0 };
                draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, key, px, py - size / 2.0, size, op, 0.0);
                ext = union(ext, (px - size, py - size, px + size, py + size));
            }
        }

        // "Puf" ao remover
        for pf in &self.poofs {
            let e = ease_out_cubic(pf.t / POOF_T);
            let (px, py) = self.uv_to_win(pf.u, pf.v);
            let size = icon * (1.0 + 0.5 * e);
            let op = (1.0 - e * 2.4).max(0.0);
            draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, &pf.key, px, py - size / 2.0, size, op, 0.0);
            for k in 0..10 {
                let ang = k as f32 * PI / 5.0 + e * 0.6;
                let dist = icon * (0.2 + 0.75 * e);
                let r = icon * 0.17 * (1.0 - e) + 3.0;
                let paint = draw::color(236, 236, 242, 0.6 * (1.0 - e));
                draw::circle(&mut self.pix, px + ang.cos() * dist, py + ang.sin() * dist * 0.8, r, &paint);
            }
            let reach = icon * 1.4;
            ext = union(ext, (px - reach, py - reach, px + reach, py + reach));
        }

        // Tooltip
        let ta = self.tooltip.alpha.value;
        if ta > 0.01 && !self.tooltip.label.is_empty() {
            let tp = ctx.text.get(&self.tooltip.label, 15.5, 0xffffff);
            let (tw, th) = (tp.width() as f32, tp.height() as f32);
            let (bw, bh) = (tw + 20.0, th + 8.0);
            let (bx, by) = self.place_box(self.tooltip.u, self.tooltip.v - (1.0 - ta) * 5.0, bw, bh);
            let (bx, by) = (bx.clamp(2.0, self.ww - bw - 2.0).round(), by.clamp(2.0, self.wh - bh - 2.0).round());
            let id = Transform::identity();
            draw::shadow_n(&mut self.pix, bx, by, bw, bh, 7.0, ta * 0.8, id, 4);
            draw::fill(&mut self.pix, &draw::rrect(bx, by, bw, bh, 7.0), &draw::color(34, 34, 38, 0.93 * ta), id);
            draw::stroke(&mut self.pix, &draw::rrect(bx + 0.5, by + 0.5, bw - 1.0, bh - 1.0, 6.5), &draw::color(255, 255, 255, 0.13 * ta), 1.0, id);
            draw::blit(&mut self.pix, tp, Transform::from_translate(bx + 10.0, by + 4.0), ta);
            ext = union(ext, (bx - 20.0, by - 20.0, bx + bw + 20.0, by + bh + 20.0));
        }

        if let Some(m) = &self.menu {
            m.draw(&mut self.pix, &mut ctx.text);
            let (mx, my, mw, mh) = m.rect();
            ext = union(ext, (mx - 30.0, my - 30.0, mx + mw + 30.0, my + mh + 30.0));
        }

        // Envia só a área que mudou (união com o quadro anterior)
        let clamp = |b: Box4| (b.0.max(0.0), b.1.max(0.0), b.2.min(self.ww), b.3.min(self.wh));
        let ext = clamp(ext);
        let up = clamp(union(ext, last));
        self.last_ext = Some(ext);
        let (x0, y0) = (up.0.floor() as i32 - 1, up.1.floor() as i32 - 1);
        let (x1, y1) = (up.2.ceil() as i32 + 1, up.3.ceil() as i32 + 1);
        if let Some(out) = &mut self.out {
            out.put(&ctx.conn, self.win, &self.pix, (x0, y0, x1 - x0, y1 - y0))?;
        }

        // Região de entrada: só a dock recebe cliques. Oculta, só uma faixa fina na borda.
        let reach = if self.s.hide != HideMode::Never && self.hide.target == 1.0 {
            2.0
        } else if self.mouse.is_some() {
            base + icon * self.s.max_scale() + 8.0
        } else {
            far
        };
        let r = self.rect_win(u0, u1, 0.0, reach, true);
        let rect = (r.0 as i32, r.1 as i32, (r.2 - r.0).ceil() as i32, (r.3 - r.1).ceil() as i32);
        if rect != self.shape {
            self.shape = rect;
            let r = Rectangle { x: rect.0 as i16, y: rect.1 as i16, width: rect.2.max(0) as u16, height: rect.3.max(0) as u16 };
            ctx.conn.shape_rectangles(shape::SO::SET, shape::SK::INPUT, ClipOrdering::UNSORTED, self.win, 0, 0, &[r])?;
        }
        Ok(())
    }
}
