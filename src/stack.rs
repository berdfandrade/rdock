//! Gavetas: uma pasta na dock que mostra os arquivos sem abrir o gerenciador de arquivos.
//!
//! - **Órbita** (até 9 itens): os arquivos saem do ícone em dois arcos; o de dentro tem os 4 mais recentes.
//! - **Painel**: grade com subpastas, busca, ordenação e prévia ao lado.
//!
//! As pastas são observadas com inotify numa thread que fica bloqueada: 0% de CPU parado.
//! Configuração: `~/.config/rdock/stacks`, uma gaveta por linha (`caminho;estilo;ordem;visto`).

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::inotify;
use tiny_skia::{Pixmap, Transform};
use x11rb::protocol::xproto::*;

use crate::anim::Spring;
use crate::config::Edge;
use crate::draw;
use crate::files;
use crate::x::{self, Keymap};
use crate::{apps, Ctx, Msg, R};

// ---------------------------------------------------------------- dados

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Style {
    Auto,
    Orbit,
    Panel,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sort {
    Recent,
    Name,
    Kind,
}

pub struct Stack {
    pub path: PathBuf,
    pub style: Style,
    pub sort: Sort,
    /// Quando a gaveta foi aberta pela última vez (segundos Unix).
    pub seen: u64,
    /// Arquivos que chegaram desde então (selo na dock).
    pub fresh: usize,
    /// Chave do ícone na dock.
    pub icon: String,
    wd: i32,
}

pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub dir: bool,
    pub mtime: u64,
    pub size: u64,
    pub mime: String,
    pub icon: String,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn file() -> PathBuf {
    crate::config::user_theme_dir().with_file_name("stacks")
}

impl Stack {
    pub fn name(&self) -> String {
        self.path.file_name().map_or_else(|| files::tilde(&self.path), |n| n.to_string_lossy().to_string())
    }

    pub fn new(path: PathBuf, style: Style, sort: Sort, seen: u64) -> Stack {
        let mut s = Stack { path, style, sort, seen, fresh: 0, icon: String::new(), wd: -1 };
        s.icon = s.folder_icon();
        s.refresh();
        s
    }

    fn folder_icon(&self) -> String {
        let known = [("Downloads", "folder-download"), ("Documentos", "folder-documents"), ("Documents", "folder-documents"), ("Imagens", "folder-pictures"), ("Pictures", "folder-pictures"), ("Música", "folder-music"), ("Vídeos", "folder-videos")];
        let n = self.name();
        let specific = known.iter().find(|(k, _)| *k == n).map_or("folder", |(_, i)| i);
        format!("{specific}|folder")
    }

    pub fn style_for(&self, n: usize) -> Style {
        match self.style {
            Style::Auto if n <= 9 => Style::Orbit,
            Style::Auto => Style::Panel,
            s => s,
        }
    }

    pub fn refresh(&mut self) {
        let seen = self.seen;
        self.fresh = list(&self.path, Sort::Recent).iter().filter(|e| e.mtime > seen).count();
    }
}

pub fn load() -> Vec<Stack> {
    let text = std::fs::read_to_string(file()).ok().unwrap_or_else(|| {
        // Primeira vez: a pasta Downloads vira uma gaveta.
        let dl = apps::home().join("Downloads");
        if dl.is_dir() { format!("{};auto;recent;{}\n", dl.display(), now()) } else { String::new() }
    });
    let mut out: Vec<Stack> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split(';');
            let path = PathBuf::from(f.next()?.trim());
            let style = match f.next() {
                Some("orbit") => Style::Orbit,
                Some("panel") => Style::Panel,
                _ => Style::Auto,
            };
            let sort = match f.next() {
                Some("name") => Sort::Name,
                Some("kind") => Sort::Kind,
                _ => Sort::Recent,
            };
            let seen = f.next().and_then(|s| s.parse().ok()).unwrap_or_else(now);
            path.is_dir().then(|| Stack::new(path, style, sort, seen))
        })
        .collect();
    out.dedup_by(|a, b| a.path == b.path);
    save(&out);
    out
}

pub fn save(stacks: &[Stack]) {
    let mut s = String::new();
    for st in stacks {
        let style = match st.style {
            Style::Auto => "auto",
            Style::Orbit => "orbit",
            Style::Panel => "panel",
        };
        let sort = match st.sort {
            Sort::Recent => "recent",
            Sort::Name => "name",
            Sort::Kind => "kind",
        };
        s += &format!("{};{style};{sort};{}\n", st.path.display(), st.seen);
    }
    let _ = std::fs::create_dir_all(file().parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(file(), s);
}

/// Conteúdo da pasta (sem ocultos), na ordem pedida.
pub fn list(dir: &Path, sort: Sort) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<Entry> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            let path = e.path();
            let md = std::fs::metadata(&path).ok()?;
            let dir = md.is_dir();
            let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            let mime = files::mime_of(&path, dir);
            let icon = if files::is_image(&mime) { format!("thumb:{}", path.display()) } else { files::icon_for(&mime) };
            Some(Entry { name, path, dir, mtime, size: md.len(), mime, icon })
        })
        .collect();
    let key = |e: &Entry| apps::normalize(&e.name);
    match sort {
        Sort::Recent => v.sort_by_key(|e| std::cmp::Reverse(e.mtime)),
        Sort::Name => v.sort_by_key(key),
        Sort::Kind => v.sort_by(|a, b| (!a.dir, &a.mime, key(a)).cmp(&(!b.dir, &b.mime, key(b)))),
    }
    v
}

/// Observa as pastas das gavetas. A thread fica bloqueada no `read` e avisa o laço principal.
pub struct Watcher {
    fd: Arc<OwnedFd>,
}

impl Watcher {
    pub fn new(tx: Sender<Msg>) -> Option<Watcher> {
        let fd = Arc::new(inotify::init(inotify::CreateFlags::CLOEXEC).ok()?);
        let rfd = fd.clone();
        std::thread::spawn(move || {
            let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
            let mut r = inotify::Reader::new(&*rfd, &mut buf);
            while let Ok(ev) = r.next() {
                if tx.send(Msg::Dir(ev.wd())).is_err() {
                    break;
                }
            }
        });
        Some(Watcher { fd })
    }

    pub fn watch(&self, st: &mut Stack) {
        use inotify::WatchFlags as F;
        let flags = F::CREATE | F::DELETE | F::MOVED_FROM | F::MOVED_TO | F::CLOSE_WRITE;
        st.wd = inotify::add_watch(&*self.fd, &st.path, flags).unwrap_or(-1);
    }

    pub fn unwatch(&self, st: &Stack) {
        if st.wd >= 0 {
            let _ = inotify::remove_watch(&*self.fd, st.wd);
        }
    }
}

pub fn by_wd(stacks: &[Stack], wd: i32) -> Option<usize> {
    stacks.iter().position(|s| s.wd == wd)
}

// ---------------------------------------------------------------- popover

/// Onde a gaveta está na tela: centro do ícone, ponto logo além da prateleira e a borda da dock.
#[derive(Clone, Copy)]
pub struct Anchor {
    pub icon: (f32, f32),
    pub edge_pt: (f32, f32),
    pub edge: Edge,
}

const TILE: f32 = 64.0;
const R_IN: f32 = 178.0;
const R_OUT: f32 = 300.0;
const STAGGER: f32 = 0.018;
const PW: f32 = 780.0;
const PH: f32 = 540.0;
const M: f32 = 30.0;
const GRID_W: f32 = 488.0;
const CELL: f32 = 114.0;
const COLS: usize = 4;
const TOP: f32 = 116.0;
const FOOT: f32 = 52.0;
const ACCENT: (u8, u8, u8) = (10, 132, 255);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Orbit,
    Panel,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Item(usize),
    Back,
    Tab(usize),
    Search,
    OpenFolder,
    ToPanel,
    Inside,
}

pub struct StackView {
    pub win: Window,
    out: x::Surface,
    pix: Pixmap,
    keymap: Keymap,
    pub visible: bool,
    closing: bool,
    pub stack: usize,
    mode: Mode,
    anchor: Anchor,
    /// Janela na tela.
    rect: (f32, f32, f32, f32),
    dir: PathBuf,
    entries: Vec<Entry>,
    /// Índices de `entries` visíveis (filtro da busca).
    shown: Vec<usize>,
    /// Arquivos mais novos que a última abertura (ponto azul).
    seen_before: u64,
    query: String,
    sel: Option<usize>,
    hover: Option<Hit>,
    big: bool,
    t: Spring,
    items: Vec<Spring>,
    elapsed: f32,
    scroll: Spring,
    bg: Option<Pixmap>,
    tint: f32,
    last_click: Option<(usize, Instant)>,
    reduce_motion: bool,
    pub needs_frame: bool,
}

impl StackView {
    pub fn new(ctx: &mut Ctx) -> R<StackView> {
        let events = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::KEY_PRESS;
        let (win, out) = x::create_window(&ctx.conn, ctx.root, ctx.visual, ctx.colormap, (0, 0, 10, 10), true, events)?;
        ctx.own.push(win);
        Ok(StackView {
            win,
            out,
            pix: Pixmap::new(10, 10).ok_or("pixmap")?,
            keymap: Keymap::load(&ctx.conn)?,
            visible: false,
            closing: false,
            stack: 0,
            mode: Mode::Orbit,
            anchor: Anchor { icon: (0.0, 0.0), edge_pt: (0.0, 0.0), edge: Edge::Bottom },
            rect: (0.0, 0.0, 10.0, 10.0),
            dir: PathBuf::new(),
            entries: Vec::new(),
            shown: Vec::new(),
            seen_before: 0,
            query: String::new(),
            sel: None,
            hover: None,
            big: false,
            t: Spring::new(0.0, 320.0, 0.82),
            items: Vec::new(),
            elapsed: 0.0,
            scroll: Spring::new(0.0, 400.0, 1.0),
            bg: None,
            tint: 0.7,
            last_click: None,
            reduce_motion: ctx.reduce_motion,
            needs_frame: false,
        })
    }

    pub fn open(&mut self, ctx: &mut Ctx, stack: usize, anchor: Anchor) -> R<()> {
        let Some(st) = ctx.stacks.get_mut(stack) else { return Ok(()) };
        self.stack = stack;
        self.anchor = anchor;
        self.dir = st.path.clone();
        self.seen_before = st.seen;
        st.seen = now();
        st.fresh = 0;
        let n = list(&self.dir, Sort::Recent).len();
        self.mode = if st.style_for(n) == Style::Orbit { Mode::Orbit } else { Mode::Panel };
        save(&ctx.stacks);
        self.query.clear();
        self.big = false;
        self.closing = false;
        self.load_dir(ctx);
        self.layout(ctx)?;
        self.t.snap(0.0);
        self.t.target = 1.0;
        self.elapsed = 0.0;
        if !self.visible {
            ctx.conn.map_window(self.win)?;
        }
        ctx.conn.configure_window(self.win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        self.visible = true;
        self.grab(ctx);
        self.needs_frame = true;
        Ok(())
    }

    pub fn closing_now(&self) -> bool {
        self.closing
    }

    pub fn close(&mut self, ctx: &mut Ctx) {
        if !self.visible || self.closing {
            return;
        }
        self.closing = true;
        self.t.target = 0.0;
        let _ = ctx.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = ctx.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        self.needs_frame = true;
    }

    /// Pasta da gaveta mudou (inotify).
    pub fn changed(&mut self, ctx: &mut Ctx, stack: usize) {
        if self.visible && !self.closing && stack == self.stack {
            self.load_dir(ctx);
            if self.mode == Mode::Orbit {
                let _ = self.layout(ctx);
            }
            self.needs_frame = true;
        }
    }

    fn grab(&self, ctx: &Ctx) {
        let c = &ctx.conn;
        let _ = c.set_input_focus(InputFocus::PARENT, self.win, x11rb::CURRENT_TIME);
        for _ in 0..20 {
            let k = c.grab_keyboard(false, self.win, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC).ok().and_then(|r| r.reply().ok());
            let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
            let p = c.grab_pointer(true, self.win, mask, GrabMode::ASYNC, GrabMode::ASYNC, x11rb::NONE, x11rb::NONE, x11rb::CURRENT_TIME).ok().and_then(|r| r.reply().ok());
            if k.is_some_and(|r| r.status == GrabStatus::SUCCESS) && p.is_some_and(|r| r.status == GrabStatus::SUCCESS) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn sort(&self, ctx: &Ctx) -> Sort {
        if self.mode == Mode::Orbit { Sort::Recent } else { ctx.stacks.get(self.stack).map_or(Sort::Recent, |s| s.sort) }
    }

    fn load_dir(&mut self, ctx: &mut Ctx) {
        self.entries = list(&self.dir, self.sort(ctx));
        if self.mode == Mode::Orbit {
            self.entries.truncate(9);
        }
        let keys: Vec<String> = self.entries.iter().map(|e| e.icon.clone()).collect();
        ctx.ensure_icons(keys);
        self.refilter();
    }

    fn refilter(&mut self) {
        let q = apps::normalize(self.query.trim());
        self.shown = (0..self.entries.len()).filter(|&i| q.is_empty() || apps::normalize(&self.entries[i].name).contains(&q)).collect();
        self.sel = if self.query.is_empty() || self.shown.is_empty() { None } else { Some(0) };
        self.scroll.target = 0.0;
        self.needs_frame = true;
    }

    /// Calcula a janela (tamanho e posição) para o modo atual.
    fn layout(&mut self, ctx: &mut Ctx) -> R<()> {
        let (mx, my, mw, mh) = (ctx.mon.0 as f32, ctx.mon.1 as f32, ctx.mon.2 as f32, ctx.mon.3 as f32);
        let a = self.anchor;
        let (x, y, w, h) = match self.mode {
            Mode::Panel => {
                let (px, py) = match a.edge {
                    Edge::Bottom => (a.icon.0 - PW / 2.0, a.edge_pt.1 - 14.0 - PH),
                    Edge::Left => (a.edge_pt.0 + 14.0, a.icon.1 - PH / 2.0),
                    Edge::Right => (a.edge_pt.0 - 14.0 - PW, a.icon.1 - PH / 2.0),
                };
                let px = px.clamp(mx + 8.0, mx + mw - PW - 8.0);
                let py = py.clamp(my + 8.0, my + mh - PH - 8.0);
                (px - M, py - M, PW + 2.0 * M, PH + 2.0 * M)
            }
            Mode::Orbit => {
                self.items = (0..self.entries.len()).map(|_| Spring::new(0.0, 260.0, 0.74)).collect();
                // Caixa que contém o ícone, os arcos (com rótulos) e o cabeçalho.
                let mut b = (a.icon.0, a.icon.1, a.icon.0, a.icon.1);
                let mut add = |r: (f32, f32, f32, f32)| b = (b.0.min(r.0), b.1.min(r.1), b.2.max(r.2), b.3.max(r.3));
                for (cx, cy) in orbit_slots(a, self.entries.len()) {
                    add((cx - 80.0, cy - TILE / 2.0 - 8.0, cx + 80.0, cy + TILE / 2.0 + 30.0));
                }
                let (hx, hy, hw, hh) = orbit_header(a, 380.0);
                add((hx, hy, hx + hw, hy + hh));
                let (x0, y0) = ((b.0 - M).max(mx), (b.1 - M).max(my));
                let (x1, y1) = ((b.2 + M).min(mx + mw), (b.3 + M).min(my + mh));
                (x0, y0, x1 - x0, y1 - y0)
            }
        };
        let (x, y, w, h) = (x.round(), y.round(), w.round(), h.round());
        // Fundo desfocado capturado antes de mostrar a janela (só no painel, que cobre uma área grande).
        self.bg = None;
        if self.mode == Mode::Panel {
            let was = self.visible;
            if was {
                let _ = ctx.conn.unmap_window(self.win);
                let _ = ctx.conn.get_input_focus().map(|c| c.reply());
            }
            if let Some((bg, lum)) = crate::launchpad::capture_blurred(ctx, (x as i32 + M as i32, y as i32 + M as i32, PW as u32, PH as u32), 0.9) {
                self.bg = Some(bg);
                // Contraste: papel de parede claro → vidro mais opaco.
                self.tint = 0.5 + 0.38 * lum;
            }
            if was {
                ctx.conn.map_window(self.win)?;
                self.grab(ctx);
            }
        }
        self.rect = (x, y, w, h);
        ctx.conn.configure_window(self.win, &ConfigureWindowAux::new().x(x as i32).y(y as i32).width(w as u32).height(h as u32))?;
        if self.pix.width() != w as u32 || self.pix.height() != h as u32 {
            self.pix = Pixmap::new(w as u32, h as u32).ok_or("pixmap")?;
        }
        Ok(())
    }

    fn to_local(&self, p: (f32, f32)) -> (f32, f32) {
        (p.0 - self.rect.0, p.1 - self.rect.1)
    }

    fn switch_to_panel(&mut self, ctx: &mut Ctx) {
        self.mode = Mode::Panel;
        self.load_dir(ctx);
        let _ = self.layout(ctx);
        self.t.snap(0.0);
        self.t.target = 1.0;
    }

    fn enter(&mut self, ctx: &mut Ctx, i: usize) {
        let e = &self.entries[i];
        if e.dir {
            self.dir = e.path.clone();
            self.query.clear();
            if self.mode == Mode::Orbit {
                self.switch_to_panel(ctx);
            } else {
                self.load_dir(ctx);
            }
        } else {
            files::open(&e.path);
            self.close(ctx);
        }
    }

    fn go_up(&mut self, ctx: &mut Ctx) {
        let root = ctx.stacks.get(self.stack).map(|s| s.path.clone()).unwrap_or_default();
        if self.dir != root {
            if let Some(p) = self.dir.parent() {
                self.dir = p.to_path_buf();
                self.load_dir(ctx);
            }
        }
    }

    // ---------------------------------------------------------------- geometria do painel (local)

    fn panel_origin(&self) -> (f32, f32) {
        (M, M)
    }

    fn tile_rect(&self, k: usize) -> (f32, f32, f32, f32) {
        let (ox, oy) = self.panel_origin();
        let (c, r) = ((k % COLS) as f32, (k / COLS) as f32);
        (ox + 16.0 + c * CELL, oy + TOP + 8.0 + r * CELL - self.scroll.value, CELL - 6.0, CELL - 6.0)
    }

    fn search_rect(&self) -> (f32, f32, f32, f32) {
        let (ox, oy) = self.panel_origin();
        (ox + PW - 236.0, oy + 14.0, 220.0, 34.0)
    }

    fn tab_rect(&self, k: usize) -> (f32, f32, f32, f32) {
        let (ox, oy) = self.panel_origin();
        (ox + 16.0 + k as f32 * 80.0, oy + 66.0, 78.0, 32.0)
    }

    fn back_rect(&self) -> (f32, f32, f32, f32) {
        let (ox, oy) = self.panel_origin();
        (ox + 16.0, oy + 14.0, 34.0, 34.0)
    }

    fn open_btn_rect(&self, ctx: &Ctx) -> (f32, f32, f32, f32) {
        let (ox, oy) = self.panel_origin();
        let w = ctx.text.width("Abrir em Arquivos", 13.0) + 28.0;
        (ox + PW - 16.0 - w, oy + PH - FOOT + 9.0, w, 34.0)
    }

    fn max_scroll(&self) -> f32 {
        let rows = self.shown.len().div_ceil(COLS) as f32;
        (rows * CELL + 16.0 - (PH - TOP - FOOT)).max(0.0)
    }

    fn header_btns(&self, ctx: &Ctx) -> [(f32, f32, f32, f32); 2] {
        let (hx, hy, hw, hh) = orbit_header(self.anchor, 380.0);
        let (hx, hy) = self.to_local((hx, hy));
        let w2 = ctx.text.width("Abrir em Arquivos", 13.0) + 26.0;
        let w1 = ctx.text.width("Painel", 13.0) + 26.0;
        let r2 = (hx + hw - 6.0 - w2, hy + 6.0, w2, hh - 12.0);
        [(r2.0 - 6.0 - w1, hy + 6.0, w1, hh - 12.0), r2]
    }

    fn hit(&self, ctx: &Ctx, x: f32, y: f32) -> Option<Hit> {
        let inr = |r: (f32, f32, f32, f32)| x >= r.0 && x <= r.0 + r.2 && y >= r.1 && y <= r.1 + r.3;
        match self.mode {
            Mode::Orbit => {
                let [b1, b2] = self.header_btns(ctx);
                if inr(b1) {
                    return Some(Hit::ToPanel);
                }
                if inr(b2) {
                    return Some(Hit::OpenFolder);
                }
                let (hx, hy, hw, hh) = orbit_header(self.anchor, 380.0);
                let (hx, hy) = self.to_local((hx, hy));
                if inr((hx, hy, hw, hh)) {
                    return Some(Hit::Inside);
                }
                orbit_slots(self.anchor, self.entries.len()).into_iter().position(|c| {
                    let (cx, cy) = self.to_local(c);
                    (x - cx).abs() <= TILE / 2.0 + 6.0 && y >= cy - TILE / 2.0 - 6.0 && y <= cy + TILE / 2.0 + 22.0
                }).map(Hit::Item)
            }
            Mode::Panel => {
                let (ox, oy) = self.panel_origin();
                if !inr((ox, oy, PW, PH)) {
                    return None;
                }
                if inr(self.back_rect()) {
                    return Some(Hit::Back);
                }
                if inr(self.search_rect()) {
                    return Some(Hit::Search);
                }
                if let Some(k) = (0..3).find(|&k| inr(self.tab_rect(k))) {
                    return Some(Hit::Tab(k));
                }
                if inr(self.open_btn_rect(ctx)) {
                    return Some(Hit::OpenFolder);
                }
                if !self.big && y > oy + TOP && y < oy + PH - FOOT && x < ox + GRID_W {
                    if let Some(k) = (0..self.shown.len()).find(|&k| inr(self.tile_rect(k))) {
                        return Some(Hit::Item(self.shown[k]));
                    }
                }
                Some(Hit::Inside)
            }
        }
    }

    // ---------------------------------------------------------------- entrada

    pub fn on_motion(&mut self, ctx: &Ctx, x: f32, y: f32) {
        let h = self.hit(ctx, x, y);
        if h != self.hover {
            self.hover = h;
            self.needs_frame = true;
        }
    }

    pub fn on_press(&mut self, ctx: &mut Ctx, button: u8, x: f32, y: f32) {
        if self.closing {
            return;
        }
        self.needs_frame = true;
        let hit = self.hit(ctx, x, y);
        match (button, hit) {
            (4 | 5, _) if self.mode == Mode::Panel => {
                let d = if button == 4 { -CELL * 0.6 } else { CELL * 0.6 };
                self.scroll.target = (self.scroll.target + d).clamp(0.0, self.max_scroll());
            }
            (1, None) => self.close(ctx),
            (1, Some(Hit::Item(i))) => {
                let double = self.last_click.is_some_and(|(j, t)| j == i && t.elapsed().as_millis() < 400);
                self.last_click = Some((i, Instant::now()));
                let k = self.shown.iter().position(|&s| s == i);
                // Na órbita um clique abre; no painel, pastas abrem com um clique e arquivos com dois.
                if self.mode == Mode::Orbit || self.entries[i].dir || double {
                    self.enter(ctx, i);
                } else {
                    self.sel = k;
                }
            }
            (1, Some(Hit::Back)) => self.go_up(ctx),
            (1, Some(Hit::Tab(k))) => {
                if let Some(st) = ctx.stacks.get_mut(self.stack) {
                    st.sort = [Sort::Recent, Sort::Name, Sort::Kind][k];
                    save(&ctx.stacks);
                }
                self.load_dir(ctx);
            }
            (1, Some(Hit::OpenFolder)) => {
                files::open(&self.dir);
                self.close(ctx);
            }
            (1, Some(Hit::ToPanel)) => self.switch_to_panel(ctx),
            (1, Some(Hit::Search | Hit::Inside)) => self.big = false,
            _ => {}
        }
    }

    pub fn on_key(&mut self, ctx: &mut Ctx, code: u8, state: KeyButMask) {
        let shift = u16::from(state) & u16::from(KeyButMask::SHIFT) != 0;
        let ks = self.keymap.keysym(code, shift);
        self.needs_frame = true;
        let n = self.shown.len();
        let mv = |sel: Option<usize>, d: i32| -> Option<usize> {
            if n == 0 {
                return None;
            }
            Some(sel.map_or(0, |s| (s as i32 + d).clamp(0, n as i32 - 1) as usize))
        };
        let cols = if self.mode == Mode::Panel { COLS as i32 } else { 4 };
        match ks {
            0xff1b => {
                if self.big {
                    self.big = false;
                } else if !self.query.is_empty() {
                    self.query.clear();
                    self.refilter();
                } else {
                    self.close(ctx);
                }
            }
            0xff08 => {
                if self.query.pop().is_some() {
                    self.refilter();
                } else {
                    self.go_up(ctx);
                }
            }
            0xff0d | 0xff8d => {
                if let Some(k) = self.sel.or(if n == 1 { Some(0) } else { None }) {
                    self.enter(ctx, self.shown[k]);
                }
            }
            0x20 if self.query.is_empty() => {
                if self.mode == Mode::Orbit {
                    self.switch_to_panel(ctx);
                } else if self.sel.is_some() {
                    self.big = !self.big;
                }
            }
            0xff51 => self.sel = mv(self.sel, -1),
            0xff53 => self.sel = mv(self.sel, 1),
            0xff52 => self.sel = mv(self.sel, -cols),
            0xff54 => self.sel = mv(self.sel, cols),
            _ => {
                if let Some(c) = x::keysym_char(ks).filter(|c| !c.is_control()) {
                    if self.mode == Mode::Orbit {
                        self.switch_to_panel(ctx);
                    }
                    self.query.push(c);
                    self.refilter();
                }
            }
        }
        // Mantém a seleção visível.
        if let (Mode::Panel, Some(k)) = (self.mode, self.sel) {
            let (_, ty, _, th) = self.tile_rect(k);
            let top = self.panel_origin().1 + TOP;
            let bottom = self.panel_origin().1 + PH - FOOT;
            let s = self.scroll.target;
            if ty + s - self.scroll.value < top {
                self.scroll.target = (s - (top - ty)).max(0.0);
            } else if ty + th > bottom {
                self.scroll.target = (s + ty + th - bottom).min(self.max_scroll());
            }
        }
    }

    // ---------------------------------------------------------------- animação e desenho

    pub fn tick(&mut self, ctx: &mut Ctx, dt: f32) -> bool {
        let mut anim = self.t.step(dt) | self.scroll.step(dt);
        self.elapsed += dt;
        let n = self.items.len();
        for (k, sp) in self.items.iter_mut().enumerate() {
            // Saem em cascata (18 ms entre um e outro) e recolhem na ordem inversa.
            let delay = if self.closing { (n - 1 - k) as f32 * STAGGER * 0.5 } else { k as f32 * STAGGER };
            sp.target = if !self.closing && self.elapsed >= delay { 1.0 } else if self.closing && self.elapsed >= delay { 0.0 } else { sp.target };
            anim |= sp.step(dt);
            anim |= sp.target != if self.closing { 0.0 } else { 1.0 };
        }
        if self.closing {
            let done = self.t.value < 0.02 && (self.mode == Mode::Panel || self.items.iter().all(|s| s.value < 0.02));
            if done {
                let _ = ctx.conn.unmap_window(self.win);
                self.visible = false;
                self.closing = false;
                self.bg = None;
                return false;
            }
            return true;
        }
        anim
    }

    pub fn render(&mut self, ctx: &mut Ctx) -> R<()> {
        if !self.visible {
            return Ok(());
        }
        self.pix.fill(tiny_skia::Color::TRANSPARENT);
        match self.mode {
            Mode::Orbit => self.draw_orbit(ctx),
            Mode::Panel => self.draw_panel(ctx),
        }
        let (w, h) = (self.pix.width() as i32, self.pix.height() as i32);
        self.out.put(&ctx.conn, self.win, &self.pix, (0, 0, w, h))
    }

    fn draw_orbit(&mut self, ctx: &mut Ctx) {
        let t = self.t.value.clamp(0.0, 1.0);
        let a = self.anchor;
        let (ax, ay) = self.to_local(a.icon);
        let slots = orbit_slots(a, self.entries.len());
        let focus = match (self.hover, self.sel) {
            (Some(Hit::Item(i)), _) => Some(i),
            (_, Some(k)) => self.shown.get(k).copied(),
            _ => None,
        };
        // Linhas pontilhadas dos arcos (discretas).
        for (r, from, to) in [(R_IN, 0, 4.min(slots.len())), (R_OUT, 4, slots.len())] {
            if to > from + 1 {
                let p = self.items[from..to].iter().map(|s| s.value).fold(0.0, f32::max).clamp(0.0, 1.0);
                let (c0, c1) = (self.to_local(slots[from]), self.to_local(slots[to - 1]));
                let ang = |c: (f32, f32)| (c.1 - ay).atan2(c.0 - ax);
                let (a0, a1) = (ang(c0), ang(c1));
                let steps = (r * (a1 - a0).abs() / 9.0) as usize;
                for s in 0..=steps {
                    let th = a0 + (a1 - a0) * s as f32 / steps.max(1) as f32;
                    draw::circle(&mut self.pix, ax + r * th.cos(), ay + r * th.sin(), 1.1, &draw::color(255, 255, 255, 0.22 * p));
                }
            }
        }
        for (i, &slot) in slots.iter().enumerate() {
            let p = self.items[i].value;
            let pa = p.clamp(0.0, 1.0);
            if pa < 0.01 {
                continue;
            }
            let (tx, ty) = self.to_local(slot);
            let (cx, cy, s) = if self.reduce_motion { (tx, ty, 1.0) } else { (ax + (tx - ax) * p, ay + (ty - ay) * p, 0.4 + 0.6 * p) };
            let e = &self.entries[i];
            let size = TILE * s;
            let focused = focus == Some(i);
            let id = Transform::identity();
            draw::shadow_n(&mut self.pix, cx - size / 2.0, cy - size / 2.0, size, size, 14.0 * s, pa * 0.8, id, 4);
            draw::fill(&mut self.pix, &draw::rrect(cx - size / 2.0, cy - size / 2.0, size, size, 14.0 * s), &draw::color(44, 38, 50, 0.86 * pa), id);
            let ring = if focused { draw::color(ACCENT.0, ACCENT.1, ACCENT.2, pa) } else { draw::color(255, 255, 255, 0.13 * pa) };
            draw::stroke(&mut self.pix, &draw::rrect(cx - size / 2.0 + 0.5, cy - size / 2.0 + 0.5, size - 1.0, size - 1.0, 14.0 * s), &ring, if focused { 2.0 } else { 1.0 }, id);
            let isz = (size * if e.icon.starts_with("thumb:") { 0.8 } else { 0.66 }).round();
            draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, &e.icon, cx, cy - isz / 2.0, isz, pa, 0.0);
            if e.mtime > self.seen_before {
                draw::circle(&mut self.pix, cx + size / 2.0 - 3.0, cy - size / 2.0 + 3.0, 4.5, &draw::color(ACCENT.0, ACCENT.1, ACCENT.2, pa));
            }
            // Rótulo: encurtado; completo numa pílula azul quando em foco.
            let label = if focused { e.name.clone() } else { ctx.text.ellipsize(&e.name, 11.5, 104.0) };
            let tp = ctx.text.get(&label, 11.5, 0xffffff);
            let (lw, lh) = (tp.width() as f32, tp.height() as f32);
            let (lx, ly) = ((cx - lw / 2.0).round(), (cy + size / 2.0 + 6.0).round());
            if focused {
                draw::fill(&mut self.pix, &draw::rrect(lx - 7.0, ly - 1.0, lw + 14.0, lh + 2.0, 6.0), &draw::color(ACCENT.0, ACCENT.1, ACCENT.2, pa), id);
            } else {
                let sh = ctx.text.get(&label, 11.5, 0x000000);
                draw::blit_fast(&mut self.pix, sh, lx as i32, ly as i32 + 1, 0.7 * pa);
            }
            let tp = ctx.text.get(&label, 11.5, 0xffffff);
            draw::blit_fast(&mut self.pix, tp, lx as i32, ly as i32, pa);
        }
        // Cabeçalho: nome · quantidade, botões Painel e Abrir em Arquivos.
        let (hx, hy, hw, hh) = orbit_header(a, 380.0);
        let (hx, hy) = self.to_local((hx, hy));
        draw::glass(&mut self.pix, None, (0.0, 0.0), hx, hy, hw, hh, 18.0, 0.0, t, Transform::identity());
        let name = ctx.stacks.get(self.stack).map(|s| s.name()).unwrap_or_default();
        let title = format!("{name} · {}", items_label(list_len(&self.dir)));
        let tp = ctx.text.get(&title, 13.0, 0xffffff);
        let tw = tp.width() as i32;
        draw::blit_fast(&mut self.pix, tp, (hx + 14.0) as i32, (hy + (hh - tp.height() as f32) / 2.0) as i32, t);
        let _ = tw;
        let [b1, b2] = self.header_btns(ctx);
        self.button(ctx, b1, "Painel", false, self.hover == Some(Hit::ToPanel), t);
        self.button(ctx, b2, "Abrir em Arquivos", true, self.hover == Some(Hit::OpenFolder), t);
    }

    fn button(&mut self, ctx: &mut Ctx, r: (f32, f32, f32, f32), label: &str, primary: bool, hover: bool, a: f32) {
        let c = if primary { draw::color(ACCENT.0, ACCENT.1, ACCENT.2, a * if hover { 1.0 } else { 0.92 }) } else { draw::color(255, 255, 255, a * if hover { 0.22 } else { 0.13 }) };
        draw::fill(&mut self.pix, &draw::rrect(r.0, r.1, r.2, r.3, 9.0), &c, Transform::identity());
        let tp = ctx.text.get(label, 13.0, 0xffffff);
        let (x, y) = (r.0 + (r.2 - tp.width() as f32) / 2.0, r.1 + (r.3 - tp.height() as f32) / 2.0);
        draw::blit_fast(&mut self.pix, tp, x.round() as i32, y.round() as i32, a);
    }

    fn draw_panel(&mut self, ctx: &mut Ctx) {
        let t = self.t.value;
        let a = t.clamp(0.0, 1.0);
        let (ox, oy) = self.panel_origin();
        // Cresce a partir da seta que aponta para o ícone.
        let (tipx, tipy) = self.caret_tip();
        let s = if self.reduce_motion { 1.0 } else { 0.94 + 0.06 * t };
        let tf = Transform::from_translate(tipx, tipy).pre_scale(s, s).pre_translate(-tipx, -tipy);
        draw::glass(&mut self.pix, self.bg.as_ref(), (ox, oy), ox, oy, PW, PH, 18.0, self.tint, a, tf);
        self.draw_caret(a, tf);
        if a < 0.05 {
            return;
        }
        // O conteúdo é desenhado sem escala (só aparece quando o painel já quase assentou).
        let ca = ((t - 0.35) / 0.65).clamp(0.0, 1.0);
        let id = Transform::identity();
        let root = ctx.stacks.get(self.stack).map(|s| s.path.clone()).unwrap_or_default();
        // Cabeçalho
        let br = self.back_rect();
        let can_back = self.dir != root;
        draw::fill(&mut self.pix, &draw::rrect(br.0, br.1, br.2, br.3, 9.0), &draw::color(255, 255, 255, if self.hover == Some(Hit::Back) && can_back { 0.2 } else { 0.1 } * ca), id);
        let chev = ctx.text.get("‹", 20.0, 0xffffff);
        draw::blit_fast(&mut self.pix, chev, (br.0 + (br.2 - chev.width() as f32) / 2.0) as i32, (br.1 + (br.3 - chev.height() as f32) / 2.0 - 1.0) as i32, ca * if can_back { 0.9 } else { 0.3 });
        let title = self.dir.file_name().map_or_else(|| files::tilde(&self.dir), |n| n.to_string_lossy().to_string());
        let tp = ctx.text.get(&title, 14.5, 0xffffff);
        let tw = (tp.width() as f32).min(PW - 330.0);
        draw::fill(&mut self.pix, &draw::rrect(br.0 + br.2 + 8.0, br.1, tw + 22.0, br.3, 9.0), &draw::color(255, 255, 255, 0.1 * ca), id);
        let tp = ctx.text.get(&title, 14.5, 0xffffff);
        draw::blit_fast(&mut self.pix, tp, (br.0 + br.2 + 19.0) as i32, (br.1 + (br.3 - tp.height() as f32) / 2.0) as i32, ca);
        // Busca
        let (sx, sy, sw, sh) = self.search_rect();
        draw::fill(&mut self.pix, &draw::rrect(sx, sy, sw, sh, 9.0), &draw::color(0, 0, 0, 0.25 * ca), id);
        draw::stroke(&mut self.pix, &draw::rrect(sx + 0.5, sy + 0.5, sw - 1.0, sh - 1.0, 8.5), &draw::color(255, 255, 255, if self.query.is_empty() { 0.14 } else { 0.5 } * ca), 1.0, id);
        draw::stroke(&mut self.pix, &tiny_skia::PathBuilder::from_circle(sx + 16.0, sy + sh / 2.0 - 2.0, 5.0), &draw::color(255, 255, 255, 0.7 * ca), 1.5, id);
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(sx + 19.8, sy + sh / 2.0 + 1.8);
        pb.line_to(sx + 23.5, sy + sh / 2.0 + 5.5);
        draw::stroke(&mut self.pix, &pb.finish(), &draw::color(255, 255, 255, 0.7 * ca), 1.7, id);
        let (q, qa) = if self.query.is_empty() { ("Buscar".to_string(), 0.5) } else { (self.query.clone(), 1.0) };
        let tp = ctx.text.get(&q, 13.0, 0xffffff);
        draw::blit_fast(&mut self.pix, tp, (sx + 30.0) as i32, (sy + (sh - tp.height() as f32) / 2.0) as i32, qa * ca);
        // Ordenação e contagem
        let sort = self.sort(ctx);
        for (k, l) in ["Recentes", "Nome", "Tipo"].iter().enumerate() {
            let r = self.tab_rect(k);
            let on = [Sort::Recent, Sort::Name, Sort::Kind][k] == sort;
            let hv = self.hover == Some(Hit::Tab(k));
            if on || hv {
                draw::fill(&mut self.pix, &draw::rrect(r.0, r.1, r.2, r.3, 8.0), &draw::color(255, 255, 255, if on { 0.18 } else { 0.08 } * ca), id);
            }
            let tp = ctx.text.get(l, 12.5, 0xffffff);
            draw::blit_fast(&mut self.pix, tp, (r.0 + (r.2 - tp.width() as f32) / 2.0) as i32, (r.1 + (r.3 - tp.height() as f32) / 2.0) as i32, ca * if on { 1.0 } else { 0.75 });
        }
        let count = items_label(self.shown.len());
        let tp = ctx.text.get(&count, 12.0, 0xffffff);
        draw::blit_fast(&mut self.pix, tp, (ox + PW - 16.0 - tp.width() as f32) as i32, (oy + 82.0 - tp.height() as f32 / 2.0) as i32, 0.6 * ca);
        for (x, y, w, h) in [(ox, oy + TOP - 6.0, PW, 1.0), (ox + GRID_W, oy + TOP - 6.0, 1.0, PH - TOP - FOOT + 6.0), (ox, oy + PH - FOOT, PW, 1.0)] {
            if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
                self.pix.fill_rect(r, &draw::color(255, 255, 255, 0.08 * ca), id, None);
            }
        }
        // Grade (recortada entre o cabeçalho e o rodapé)
        let (clip0, clip1) = (oy + TOP - 5.0, oy + PH - FOOT);
        let sel_entry = self.sel.and_then(|k| self.shown.get(k).copied());
        for k in 0..self.shown.len() {
            let (x, y, w, h) = self.tile_rect(k);
            if y + h < clip0 || y > clip1 {
                continue;
            }
            let i = self.shown[k];
            // Ao rolar, os itens somem suavemente nas bordas da área.
            let fade = ((y + h - clip0) / 24.0).min((clip1 - y) / 24.0).clamp(0.0, 1.0) * ca;
            let hv = self.hover == Some(Hit::Item(i));
            let inside = y >= clip0 - 1.0 && y + h <= clip1 + 1.0;
            if sel_entry == Some(i) && inside {
                draw::fill(&mut self.pix, &draw::rrect(x, y, w, h, 14.0), &draw::color(255, 255, 255, 0.16 * fade), id);
                draw::stroke(&mut self.pix, &draw::rrect(x + 1.0, y + 1.0, w - 2.0, h - 2.0, 13.0), &draw::color(ACCENT.0, ACCENT.1, ACCENT.2, fade), 2.0, id);
            } else if hv && inside {
                draw::fill(&mut self.pix, &draw::rrect(x, y, w, h, 14.0), &draw::color(255, 255, 255, 0.08 * fade), id);
            }
            let e = &self.entries[i];
            let isz = if e.icon.starts_with("thumb:") { 60.0 } else { 52.0 };
            let clip = (clip0 as i32, clip1 as i32);
            draw::icon_clip(&mut self.pix, &mut ctx.scaled, &ctx.icons, &e.icon, x + w / 2.0, y + 10.0 + (52.0 - isz) / 2.0, isz, fade, 0.0, clip);
            let lines = wrap2(&ctx.text, &e.name, 11.5, w - 10.0);
            for (n, l) in lines.iter().enumerate() {
                let tp = ctx.text.get(l, 11.5, 0xffffff);
                let lx = x + (w - tp.width() as f32) / 2.0;
                let ly = y + 66.0 + n as f32 * (tp.height() as f32 - 2.0);
                draw::blit_fast_clip(&mut self.pix, tp, lx.round() as i32, ly.round() as i32, fade, clip);
            }
        }
        if self.shown.is_empty() {
            let msg = if self.query.is_empty() { "Pasta vazia" } else { "Nenhum resultado" };
            let tp = ctx.text.get(msg, 14.0, 0xffffff);
            draw::blit_fast(&mut self.pix, tp, (ox + (GRID_W - tp.width() as f32) / 2.0) as i32, (oy + TOP + 100.0) as i32, 0.55 * ca);
        }
        self.draw_preview(ctx, sel_entry, ca);
        // Rodapé
        let hint = "Setas navegam · Enter abre · Espaço: prévia · Backspace volta";
        let tp = ctx.text.get(hint, 11.5, 0xffffff);
        draw::blit_fast(&mut self.pix, tp, (ox + 16.0) as i32, (oy + PH - FOOT / 2.0 - tp.height() as f32 / 2.0) as i32, 0.55 * ca);
        let r = self.open_btn_rect(ctx);
        self.button(ctx, r, "Abrir em Arquivos", false, self.hover == Some(Hit::OpenFolder), ca);
    }

    fn draw_preview(&mut self, ctx: &mut Ctx, sel: Option<usize>, a: f32) {
        let (ox, oy) = self.panel_origin();
        let id = Transform::identity();
        let (px, pw) = (ox + GRID_W, PW - GRID_W);
        let (py, ph) = (oy + TOP, PH - TOP - FOOT);
        let center = |text: &mut draw::Text, pix: &mut Pixmap, s: &str, size: f32, y: f32, alpha: f32, x0: f32, w: f32| {
            let s = text.ellipsize(s, size, w - 32.0);
            let tp = text.get(&s, size, 0xffffff);
            draw::blit_fast(pix, tp, (x0 + (w - tp.width() as f32) / 2.0) as i32, y as i32, alpha);
            tp.height() as f32
        };
        if self.big {
            if let Some(i) = sel {
                // Prévia grande sobre a grade.
                let (bx, by, bw, bh) = (ox + 12.0, py + 4.0, PW - 24.0, ph - 8.0);
                draw::fill(&mut self.pix, &draw::rrect(bx, by, bw, bh, 14.0), &draw::color(20, 18, 24, 0.94 * a), id);
                let e = &self.entries[i];
                let size = if e.icon.starts_with("thumb:") { (bh - 70.0).min(bw - 40.0) } else { 160.0 };
                draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, &e.icon, bx + bw / 2.0, by + 16.0 + (bh - 70.0 - size) / 2.0, size, a, 0.0);
                let name = e.name.clone();
                center(&mut ctx.text, &mut self.pix, &name, 14.0, by + bh - 44.0, a, bx, bw);
                return;
            }
        }
        let (icon, name, lines) = match sel.map(|i| &self.entries[i]) {
            Some(e) => {
                let kind = if e.dir { "Pasta".to_string() } else { format!("{} · {}", kind_label(&e.mime), human_size(e.size)) };
                (e.icon.clone(), e.name.clone(), vec![kind, format!("Modificado {}", ago(e.mtime))])
            }
            None => {
                let name = self.dir.file_name().map_or_else(|| files::tilde(&self.dir), |n| n.to_string_lossy().to_string());
                ("folder".to_string(), name, vec!["Selecione um arquivo para ver a".into(), "prévia. Espaço abre em tamanho".into(), "grande.".into()])
            }
        };
        let size = if icon.starts_with("thumb:") { 180.0 } else { 88.0 };
        let top = py + (ph - size - 40.0 - lines.len() as f32 * 18.0) / 2.0;
        draw::icon(&mut self.pix, &mut ctx.scaled, &ctx.icons, &icon, px + pw / 2.0, top, size, a, 0.0);
        let mut y = top + size + 14.0;
        y += center(&mut ctx.text, &mut self.pix, &name, 14.5, y, a, px, pw) + 4.0;
        for l in lines {
            y += center(&mut ctx.text, &mut self.pix, &l, 11.5, y, 0.6 * a, px, pw) - 2.0;
        }
    }

    /// Ponta da seta do painel, apontando para o ícone (coordenadas locais).
    fn caret_tip(&self) -> (f32, f32) {
        let (ix, iy) = self.to_local(self.anchor.icon);
        let (ox, oy) = self.panel_origin();
        match self.anchor.edge {
            Edge::Bottom => (ix.clamp(ox + 30.0, ox + PW - 30.0), oy + PH + 11.0),
            Edge::Left => (ox - 11.0, iy.clamp(oy + 30.0, oy + PH - 30.0)),
            Edge::Right => (ox + PW + 11.0, iy.clamp(oy + 30.0, oy + PH - 30.0)),
        }
    }

    fn draw_caret(&mut self, a: f32, tf: Transform) {
        let (tx, ty) = self.caret_tip();
        let mut pb = tiny_skia::PathBuilder::new();
        let k = 12.0;
        match self.anchor.edge {
            Edge::Bottom => {
                pb.move_to(tx - k, ty - k);
                pb.line_to(tx, ty);
                pb.line_to(tx + k, ty - k);
            }
            Edge::Left => {
                pb.move_to(tx + k, ty - k);
                pb.line_to(tx, ty);
                pb.line_to(tx + k, ty + k);
            }
            Edge::Right => {
                pb.move_to(tx - k, ty - k);
                pb.line_to(tx, ty);
                pb.line_to(tx - k, ty + k);
            }
        }
        pb.close();
        draw::fill(&mut self.pix, &pb.finish(), &draw::color(34, 32, 40, 0.96 * a), tf);
    }
}

/// Centros (na tela) dos itens da órbita: 4 no arco de dentro, o resto no de fora.
fn orbit_slots(a: Anchor, n: usize) -> Vec<(f32, f32)> {
    let away: f32 = match a.edge {
        Edge::Bottom => -std::f32::consts::FRAC_PI_2,
        Edge::Left => 0.0,
        Edge::Right => std::f32::consts::PI,
    };
    let arc = |count: usize, r: f32, spread: f32| -> Vec<(f32, f32)> {
        (0..count)
            .map(|k| {
                let f = if count == 1 { 0.0 } else { -1.0 + 2.0 * k as f32 / (count - 1) as f32 };
                let th = away + spread.to_radians() * f;
                (a.icon.0 + r * th.cos(), a.icon.1 + r * th.sin())
            })
            .collect()
    };
    let inner = n.min(4);
    let mut v = arc(inner, R_IN, if inner <= 2 { 22.0 } else { 58.0 });
    if n > 4 {
        let outer = n - 4;
        v.extend(arc(outer, R_OUT, if outer <= 2 { 22.0 } else { 34.0 + 6.0 * outer as f32 }));
    }
    v
}

/// Pílula do cabeçalho da órbita, além do arco de fora (na tela).
fn orbit_header(a: Anchor, w: f32) -> (f32, f32, f32, f32) {
    let h = 44.0;
    let d = R_OUT + TILE / 2.0 + 34.0;
    match a.edge {
        Edge::Bottom => (a.icon.0 - w / 2.0, a.icon.1 - d - h, w, h),
        Edge::Left => (a.icon.0 + d, a.icon.1 - h / 2.0, w, h),
        Edge::Right => (a.icon.0 - d - w, a.icon.1 - h / 2.0, w, h),
    }
}

fn items_label(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} itens") }
}

fn list_len(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |r| r.filter_map(|e| e.ok()).filter(|e| !e.file_name().to_string_lossy().starts_with('.')).count())
}

/// Quebra o nome em até duas linhas que cabem em `w` (a segunda com "…" se precisar).
fn wrap2(text: &draw::Text, s: &str, size: f32, w: f32) -> Vec<String> {
    if text.width(s, size) <= w {
        return vec![s.to_string()];
    }
    let chars: Vec<char> = s.chars().collect();
    let mut cut = chars.len();
    while cut > 1 && text.width(&chars[..cut].iter().collect::<String>(), size) > w {
        cut -= 1;
    }
    // Prefere quebrar depois de um espaço, hífen ou ponto.
    if let Some(p) = chars[..cut].iter().rposition(|c| matches!(c, ' ' | '-' | '_' | '.')).filter(|&p| p > cut / 2) {
        cut = p + 1;
    }
    let first: String = chars[..cut].iter().collect();
    let rest: String = chars[cut..].iter().collect();
    vec![first.trim_end().to_string(), text.ellipsize(rest.trim_start(), size, w)]
}

fn kind_label(mime: &str) -> String {
    let sub = mime.split('/').nth(1).unwrap_or(mime);
    let sub = sub.trim_start_matches("x-").trim_start_matches("vnd.");
    match mime.split('/').next() {
        Some("image") => format!("Imagem {}", sub.to_uppercase()),
        Some("video") => "Vídeo".into(),
        Some("audio") => "Áudio".into(),
        Some("text") if sub == "plain" => "Texto simples".into(),
        Some("text") => format!("Texto {}", sub.to_uppercase()),
        _ => sub.split(['.', '-']).next().unwrap_or(sub).to_uppercase(),
    }
}

fn human_size(b: u64) -> String {
    match b {
        0..=999 => format!("{b} B"),
        1000..=999_999 => format!("{:.0} KB", b as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1} MB", b as f64 / 1e6),
        _ => format!("{:.1} GB", b as f64 / 1e9),
    }
}

fn ago(t: u64) -> String {
    let d = now().saturating_sub(t);
    match d {
        0..=59 => "agora".into(),
        60..=3599 => format!("há {} min", d / 60),
        3600..=86_399 => format!("há {} h", d / 3600),
        86_400..=2_591_999 => format!("há {} dias", d / 86_400),
        _ => format!("há {} meses", d / 2_592_000),
    }
}
