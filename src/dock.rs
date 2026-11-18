//! A dock: layout com magnificação "olho de peixe", bounce ao abrir, indicadores de execução,
//! tooltips, menu de contexto e arrastar para reordenar / remover (com "puf").

use std::collections::HashMap;
use std::f32::consts::PI;
use std::time::{Duration, Instant};

use tiny_skia::{Color, IntRect, Pixmap, Transform};
use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

use crate::anim::{ease_out_cubic, Spring};
use crate::draw;
use crate::menu::Menu;
use crate::{config, x, Action, Ctx, R};

const ICON: f32 = 52.0;
const MAX_SCALE: f32 = 1.85;
const GAP: f32 = 8.0;
const HPAD: f32 = 6.0;
const PAD_TOP: f32 = 7.0;
const PAD_BOTTOM: f32 = 9.0;
const SHELF_H: f32 = ICON + PAD_TOP + PAD_BOTTOM;
const MARGIN: f32 = 6.0;
const WIN_H: i32 = 260;
const SEP_W: f32 = 17.0;
const RADIUS: f32 = 18.0;
/// Margem ao redor da prateleira no cache 9-slice (espaço para a sombra).
const SHELF_PAD: f32 = 20.0;
const INFLUENCE: f32 = ICON * 3.1;
const BOUNCE_T: f32 = 0.62;
const BOUNCE_H: f32 = ICON * 0.62;
const BOUNCE_MAX: f32 = 12.0;
const REMOVE_DIST: f32 = 60.0;
const DRAG_THRESHOLD: f32 = 6.0;
const POOF_T: f32 = 0.45;
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
    flip_x: Spring,
    flip_y: Spring,
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
    x: f32,
    y: f32,
}

struct Drag {
    uid: u64,
    x: f32,
    y: f32,
    off_x: f32,
    off_y: f32,
    remove: bool,
}

struct Poof {
    x: f32,
    y: f32,
    t: f32,
    key: String,
}

struct Tooltip {
    uid: Option<u64>,
    label: String,
    alpha: Spring,
    x: f32,
    y: f32,
}

pub struct Dock {
    pub win: Window,
    gc: Gcontext,
    pix: Pixmap,
    w: f32,
    items: Vec<Item>,
    next_uid: u64,
    mouse: Option<(f32, f32)>,
    press: Option<Press>,
    drag: Option<Drag>,
    menu: Option<Menu>,
    tooltip: Tooltip,
    poofs: Vec<Poof>,
    last_ext: (f32, f32, f32),
    shape: (i32, i32, i32, i32),
    trash_full: bool,
    shelf: [Pixmap; 3],
    pub needs_frame: bool,
}

fn mag(d: f32) -> f32 {
    let t = (d.abs() / INFLUENCE).min(1.0);
    1.0 + (MAX_SCALE - 1.0) * (0.5 + 0.5 * (PI * t).cos())
}

fn shelf_bottom() -> f32 {
    WIN_H as f32 - MARGIN
}
fn shelf_top() -> f32 {
    shelf_bottom() - SHELF_H
}
fn baseline() -> f32 {
    shelf_bottom() - PAD_BOTTOM
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

/// Renderiza a prateleira (sombra, gradiente, bordas) uma única vez e corta em esquerda/meio/direita.
fn build_shelf() -> [Pixmap; 3] {
    let cap = (RADIUS + SHELF_PAD + 2.0).ceil();
    let w = 2.0 * cap + 1.0;
    let h = SHELF_H + 2.0 * SHELF_PAD;
    let mut p = Pixmap::new(w as u32, h as u32).unwrap();
    let id = Transform::identity();
    let (x, y, sw) = (SHELF_PAD, SHELF_PAD, w - 2.0 * SHELF_PAD);
    draw::shadow(&mut p, x, y, sw, SHELF_H, RADIUS, 1.0, id);
    let paint = crate::icons::gradient_paint(Color::from_rgba8(78, 78, 88, 150), Color::from_rgba8(36, 36, 42, 170), y, y + SHELF_H);
    draw::fill(&mut p, &draw::rrect(x, y, sw, SHELF_H, RADIUS), &paint, id);
    draw::stroke(&mut p, &draw::rrect(x - 0.5, y - 0.5, sw + 1.0, SHELF_H + 1.0, RADIUS + 0.5), &draw::color(0, 0, 0, 0.35), 1.0, id);
    draw::stroke(&mut p, &draw::rrect(x + 0.5, y + 0.5, sw - 1.0, SHELF_H - 1.0, RADIUS - 0.5), &draw::color(255, 255, 255, 0.16), 1.0, id);
    let cut = |x: f32, cw: f32| p.clone_rect(IntRect::from_xywh(x as i32, 0, cw as u32, h as u32).unwrap()).unwrap();
    [cut(0.0, cap), cut(cap, 1.0), cut(cap + 1.0, cap)]
}

pub fn trash_is_full() -> bool {
    std::fs::read_dir(crate::apps::home().join(".local/share/Trash/files"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

impl Dock {
    pub fn new(ctx: &mut Ctx) -> R<Dock> {
        let conn = ctx.conn.clone();
        let (mx, my, mw, mh) = ctx.mon;
        let wa = x::prop32(&conn, ctx.root, ctx.atoms._NET_WORKAREA, AtomEnum::CARDINAL);
        let inset = if wa.len() >= 4 { ((my + mh) - (wa[1] as i32 + wa[3] as i32)).clamp(0, 300) } else { 0 };
        let geom = (mx, my + mh - inset - WIN_H, mw, WIN_H);
        let events = EventMask::EXPOSURE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::ENTER_WINDOW
            | EventMask::LEAVE_WINDOW;
        let (win, gc) = x::create_window(&conn, ctx.root, ctx.visual, ctx.colormap, geom, false, events)?;
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
        let strut = (inset as f32 + SHELF_H + MARGIN) as u32;
        conn.change_property32(PropMode::REPLACE, win, a._NET_WM_STRUT, AtomEnum::CARDINAL, &[0, 0, 0, strut])?;
        conn.change_property32(
            PropMode::REPLACE,
            win,
            a._NET_WM_STRUT_PARTIAL,
            AtomEnum::CARDINAL,
            &[0, 0, 0, strut, 0, 0, 0, 0, 0, 0, mx as u32, (mx + mw - 1) as u32],
        )?;
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
        conn.shape_rectangles(shape::SO::SET, shape::SK::INPUT, ClipOrdering::UNSORTED, win, 0, 0, &[])?;
        conn.map_window(win)?;
        ctx.own.push(win);

        let mut dock = Dock {
            win,
            gc,
            pix: Pixmap::new(mw as u32, WIN_H as u32).ok_or("pixmap")?,
            w: mw as f32,
            items: Vec::new(),
            next_uid: 1,
            mouse: None,
            press: None,
            drag: None,
            menu: None,
            tooltip: Tooltip { uid: None, label: String::new(), alpha: Spring::new(0.0, 500.0, 1.0), x: 0.0, y: 0.0 },
            poofs: Vec::new(),
            last_ext: (0.0, mw as f32, 0.0),
            shape: (0, 0, 0, 0),
            trash_full: trash_is_full(),
            shelf: build_shelf(),
            needs_frame: true,
        };
        let mut items = vec![dock.item(Kind::Launchpad, 1.0)];
        for id in config::load_pinned(&ctx.apps) {
            if let Some(app) = ctx.apps.iter().position(|a| a.id == id) {
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
            flip_x: Spring::new(0.0, 320.0, 0.85),
            flip_y: Spring::new(0.0, 320.0, 0.85),
            press: Spring::new(0.0, 900.0, 1.0),
            bounce: None,
            launch_until: None,
            removing: false,
        }
    }

    fn index_of(&self, uid: u64) -> Option<usize> {
        self.items.iter().position(|i| i.uid == uid)
    }

    fn find_app(&self, app: usize) -> Option<usize> {
        self.items.iter().position(|i| matches!(i.kind, Kind::App { app: a, .. } if a == app) && !i.removing)
    }

    fn slot_w(it: &Item, scaled: bool) -> f32 {
        let p = it.presence.value.max(0.0);
        match it.kind {
            Kind::Sep(_) => SEP_W * p,
            _ => (ICON * if scaled { it.scale.value } else { 1.0 } + GAP) * p,
        }
    }

    /// Centros dos itens e extremidades da prateleira.
    fn layout(&self, scaled: bool) -> (Vec<f32>, f32, f32) {
        let ws: Vec<f32> = self.items.iter().map(|i| Self::slot_w(i, scaled)).collect();
        let total: f32 = ws.iter().sum::<f32>() + 2.0 * HPAD;
        let x0 = ((self.w - total) / 2.0).round();
        let mut x = x0 + HPAD;
        let centers = ws
            .iter()
            .map(|w| {
                let c = x + w / 2.0;
                x += w;
                c
            })
            .collect();
        (centers, x0, x0 + total)
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        let (cs, _, _) = self.layout(true);
        self.items.iter().enumerate().position(|(i, it)| {
            if matches!(it.kind, Kind::Sep(_)) || it.removing {
                return false;
            }
            let half = Self::slot_w(it, true) / 2.0;
            let top = baseline() - ICON * it.scale.value - 4.0;
            (x - cs[i]).abs() <= half && y >= top
        })
    }

    /// Aplica uma mudança estrutural animando os itens das posições antigas para as novas (FLIP).
    fn with_flip(&mut self, f: impl FnOnce(&mut Vec<Item>)) {
        let (cs, _, _) = self.layout(true);
        let before: HashMap<u64, f32> = self.items.iter().zip(cs).map(|(it, c)| (it.uid, c + it.flip_x.value)).collect();
        f(&mut self.items);
        let (cs, _, _) = self.layout(true);
        for (it, c) in self.items.iter_mut().zip(cs) {
            if let Some(b) = before.get(&it.uid) {
                it.flip_x.value = b - c;
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
        self.last_ext = (0.0, self.w, 0.0);
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
        let it = &mut self.items[i];
        it.bounce = Some(Bounce { t: 0.0, stop: false });
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
        self.mouse = Some((x, y));
        self.needs_frame = true;
        if let Some(m) = &mut self.menu {
            m.hovered = m.hit(x, y);
            return;
        }
        if self.drag.is_none() {
            if let Some(p) = &self.press {
                if (x - p.x).hypot(y - p.y) > DRAG_THRESHOLD {
                    self.start_drag(ctx);
                }
            }
        }
        if self.drag.is_some() {
            self.update_drag(ctx, x, y);
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
        let Some(i) = self.hit(x, y) else { return };
        match button {
            1 => {
                self.items[i].press.target = 1.0;
                self.press = Some(Press { uid: self.items[i].uid, x, y });
            }
            3 => self.open_menu(ctx, i),
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
        if self.hit(x, y) != Some(i) {
            return None;
        }
        match self.items[i].kind {
            Kind::Launchpad => Some(Action::ToggleLaunchpad),
            Kind::Trash => Some(Action::OpenTrash),
            Kind::App { app, .. } => Some(Action::Launch(app)),
            Kind::Sep(_) => None,
        }
    }

    fn open_menu(&mut self, ctx: &mut Ctx, i: usize) {
        let it = &self.items[i];
        let Kind::App { app, pinned } = it.kind else {
            return;
        };
        let running = ctx.running.contains_key(&app);
        let mut items = Vec::new();
        items.push((if running { "Mostrar" } else { "Abrir" }.to_string(), Action::Launch(app)));
        if ctx.apps[app].launchable() {
            let l = if pinned { "Remover do Dock" } else { "Manter no Dock" };
            items.push((l.to_string(), Action::TogglePin(app)));
        }
        if running {
            items.push(("Encerrar".to_string(), Action::Quit(app)));
        }
        let (cs, _, _) = self.layout(true);
        let top = baseline() - ICON * it.scale.value;
        self.menu = Some(Menu::new(items, cs[i], top - 10.0, &ctx.text, self.w));
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
        let Kind::App { app, .. } = self.items[i].kind else {
            self.items[i].press.target = 0.0;
            return;
        };
        self.items[i].press.target = 0.0;
        if !ctx.apps[app].launchable() {
            return;
        }
        let (cs, _, _) = self.layout(true);
        let size = ICON * self.items[i].scale.value;
        self.drag = Some(Drag {
            uid: p.uid,
            x: p.x,
            y: p.y,
            off_x: cs[i] - p.x,
            off_y: baseline() - size / 2.0 - p.y,
            remove: false,
        });
    }

    fn update_drag(&mut self, _ctx: &Ctx, x: f32, y: f32) {
        let Some(uid) = self.drag.as_ref().map(|d| d.uid) else { return };
        let Some(i) = self.index_of(uid) else { return };
        let pinned = matches!(self.items[i].kind, Kind::App { pinned: true, .. });
        let remove = pinned && y < shelf_top() - REMOVE_DIST;
        if let Some(d) = &mut self.drag {
            (d.x, d.y, d.remove) = (x, y, remove);
        }
        self.items[i].presence.target = if remove { 0.0 } else { 1.0 };
        if remove {
            return;
        }
        let sep0 = pos_of(&self.items, Kind::Sep(0));
        let (cs, _, _) = self.layout(true);
        if !pinned && x >= cs[sep0] {
            return;
        }
        let count = (1..sep0).filter(|&j| j != i && cs[j] < x).count();
        let target = 1 + count;
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

    fn finish_drag(&mut self, ctx: &mut Ctx) {
        let Some(d) = self.drag.take() else { return };
        let Some(i) = self.index_of(d.uid) else { return };
        let Kind::App { app, .. } = self.items[i].kind else { return };
        let size = ICON * self.items[i].scale.value;
        let cx = d.x + d.off_x;
        let cy = (d.y + d.off_y).max(size / 2.0 + 2.0);
        if d.remove {
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
                self.poofs.push(Poof { x: cx, y: cy, t: 0.0, key: ctx.apps[app].icon.clone() });
            }
        } else {
            let (cs, _, _) = self.layout(true);
            let it = &mut self.items[i];
            it.flip_x.value = cx - cs[i];
            it.flip_y.value = cy - (baseline() - size / 2.0);
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
        let has_unpinned = self.items.iter().any(|i| matches!(i.kind, Kind::App { pinned: false, .. }) && !i.removing);
        let remove_mode = self.drag.as_ref().is_some_and(|d| d.remove);
        let hover_x = match (self.mouse, &self.menu) {
            (Some((x, _)), None) if !remove_mode => Some(x),
            _ => None,
        };
        let (base, _, _) = self.layout(false);
        for (i, it) in self.items.iter_mut().enumerate() {
            if let Kind::Sep(0) = it.kind {
                it.presence.target = if has_unpinned { 1.0 } else { 0.0 };
            }
            it.scale.target = hover_x.map_or(1.0, |mx| mag(mx - base[i]));
            anim |= it.scale.step(dt);
            anim |= it.presence.step(dt);
            anim |= it.dot.step(dt);
            anim |= it.flip_x.step(dt);
            anim |= it.flip_y.step(dt);
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
            self.mouse.and_then(|(x, y)| self.hit(x, y))
        } else {
            None
        };
        match hovered {
            Some(i) => {
                self.tooltip.uid = Some(self.items[i].uid);
                self.tooltip.label = label(ctx, self.items[i].kind);
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
        let (lx0, lx1, ltop) = self.last_ext;
        draw::clear_rect(&mut self.pix, lx0.floor() as i32, ltop.floor() as i32, lx1.ceil() as i32, WIN_H);
        let (cs, x0, x1) = self.layout(true);
        let (sb, st, base) = (shelf_bottom(), shelf_top(), baseline());
        let mut ext = (x0 - 30.0, x1 + 30.0, st - SHELF_PAD);

        // Prateleira de vidro (9-slice pré-renderizado, copiado direto na memória)
        let sw = (x1 - x0).round() as i32;
        if sw as f32 > 2.0 * RADIUS {
            let [left, mid, right] = &self.shelf;
            let cap = left.width() as i32;
            let (lx, y) = ((x0 - SHELF_PAD).round() as i32, (st - SHELF_PAD) as i32);
            let inner = (sw + 2 * SHELF_PAD as i32 - 2 * cap).max(0);
            draw::copy(&mut self.pix, left, lx, y, cap);
            draw::copy(&mut self.pix, mid, lx + cap, y, inner);
            draw::copy(&mut self.pix, right, lx + cap + inner, y, cap);
        }

        let drag_uid = self.drag.as_ref().map(|d| d.uid);
        for (i, it) in self.items.iter().enumerate() {
            let p = it.presence.value.clamp(0.0, 1.0);
            let cx = cs[i] + it.flip_x.value;
            if let Kind::Sep(_) = it.kind {
                if p > 0.01 {
                    let line = tiny_skia::Rect::from_xywh(cx - 0.5, st + 11.0, 1.0, SHELF_H - 22.0);
                    if let Some(r) = line {
                        self.pix.fill_rect(r, &draw::color(255, 255, 255, 0.28 * p), Transform::identity(), None);
                    }
                }
                continue;
            }
            if drag_uid == Some(it.uid) || it.removing && it.presence.target == 0.0 && drag_uid.is_none() && p < 0.02 {
                continue;
            }
            let size = ICON * it.scale.value * (0.35 + 0.65 * p);
            let bounce = it.bounce.map_or(0.0, |b| {
                let ph = (b.t % BOUNCE_T) / BOUNCE_T;
                BOUNCE_H * 4.0 * ph * (1.0 - ph)
            });
            let bottom = base + it.flip_y.value - bounce;
            let key = icon_key(&ctx.apps, it.kind, self.trash_full);
            draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, key, cx, bottom - size, size, p, it.press.value * 0.8);
            if it.dot.value > 0.01 {
                draw::circle(&mut self.pix, cx, sb - 4.0, 2.2, &draw::color(255, 255, 255, 0.9 * it.dot.value * p));
            }
            ext.2 = ext.2.min(bottom - size - 4.0);
            if self.tooltip.uid == Some(it.uid) {
                self.tooltip.x = cx;
                self.tooltip.y = bottom - size - 10.0;
            }
        }

        // Ícone sendo arrastado
        if let Some(d) = &self.drag {
            if let Some(i) = self.index_of(d.uid) {
                let it = &self.items[i];
                let size = ICON * it.scale.value.max(1.0);
                let cx = d.x + d.off_x;
                let cy = (d.y + d.off_y).max(size / 2.0 + 2.0);
                let key = icon_key(&ctx.apps, it.kind, self.trash_full);
                let op = if d.remove { 0.55 } else { 1.0 };
                draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, key, cx, cy - size / 2.0, size, op, 0.0);
                ext = (ext.0.min(cx - size), ext.1.max(cx + size), ext.2.min(cy - size));
            }
        }

        // "Puf" ao remover
        for pf in &self.poofs {
            let e = ease_out_cubic(pf.t / POOF_T);
            let size = ICON * (1.0 + 0.5 * e);
            let op = (1.0 - e * 2.4).max(0.0);
            draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, &pf.key, pf.x, pf.y - size / 2.0, size, op, 0.0);
            for k in 0..10 {
                let ang = k as f32 * PI / 5.0 + e * 0.6;
                let dist = ICON * (0.2 + 0.75 * e);
                let r = ICON * 0.17 * (1.0 - e) + 3.0;
                let paint = draw::color(236, 236, 242, 0.6 * (1.0 - e));
                draw::circle(&mut self.pix, pf.x + ang.cos() * dist, pf.y + ang.sin() * dist * 0.8, r, &paint);
            }
            ext = (ext.0.min(pf.x - ICON * 1.4), ext.1.max(pf.x + ICON * 1.4), ext.2.min(pf.y - ICON * 1.4));
        }

        // Tooltip
        let ta = self.tooltip.alpha.value;
        if ta > 0.01 && !self.tooltip.label.is_empty() {
            let tp = ctx.text.get(&self.tooltip.label, 13.5, 0xffffff);
            let (tw, th) = (tp.width() as f32, tp.height() as f32);
            let (bw, bh) = (tw + 18.0, th + 6.0);
            let bx = (self.tooltip.x - bw / 2.0).round();
            let by = (self.tooltip.y - bh + (1.0 - ta) * 5.0).round();
            let id = Transform::identity();
            draw::shadow_n(&mut self.pix, bx, by, bw, bh, 7.0, ta * 0.8, id, 4);
            draw::fill(&mut self.pix, &draw::rrect(bx, by, bw, bh, 7.0), &draw::color(34, 34, 38, 0.93 * ta), id);
            draw::stroke(&mut self.pix, &draw::rrect(bx + 0.5, by + 0.5, bw - 1.0, bh - 1.0, 6.5), &draw::color(255, 255, 255, 0.13 * ta), 1.0, id);
            draw::blit(&mut self.pix, tp, Transform::from_translate(bx + 9.0, by + 3.0), ta);
            ext = (ext.0.min(bx - 20.0), ext.1.max(bx + bw + 20.0), ext.2.min(by - 20.0));
        }

        if let Some(m) = &self.menu {
            m.draw(&mut self.pix, &mut ctx.text);
            let (mx, my, mw, _) = m.rect();
            ext = (ext.0.min(mx - 30.0), ext.1.max(mx + mw + 30.0), ext.2.min(my - 30.0));
        }

        // Envia só a área que mudou (união com o quadro anterior)
        let ext = (ext.0.max(0.0), ext.1.min(self.w), ext.2.max(0.0));
        let dx0 = ext.0.min(lx0).floor() as i32;
        let dx1 = ext.1.max(lx1).ceil() as i32;
        let dy0 = ext.2.min(ltop).floor() as i32;
        self.last_ext = ext;
        x::put_region(&ctx.conn, self.win, self.gc, &self.pix, (dx0, dy0, dx1 - dx0, WIN_H - dy0), &mut ctx.buf)?;

        // Região de entrada: só a dock recebe cliques; o resto é transparente para o mouse.
        let top = if self.mouse.is_some() { base - ICON * MAX_SCALE - 8.0 } else { st };
        let rect = (x0 as i32, top as i32, (x1 - x0) as i32, WIN_H - top as i32);
        if rect != self.shape {
            self.shape = rect;
            let r = Rectangle { x: rect.0 as i16, y: rect.1 as i16, width: rect.2.max(0) as u16, height: rect.3.max(0) as u16 };
            ctx.conn.shape_rectangles(shape::SO::SET, shape::SK::INPUT, ClipOrdering::UNSORTED, self.win, 0, 0, &[r])?;
        }
        Ok(())
    }
}
