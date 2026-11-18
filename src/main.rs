//! rdock — uma dock no estilo do macOS para X11 (Cinnamon, XFCE, MATE, i3…), em Rust.

mod anim;
mod apps;
mod config;
mod dock;
mod draw;
mod icons;
mod launchpad;
mod menu;
mod settings;
mod x;

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

pub type R<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Launch(usize),
    Quit(usize),
    TogglePin(usize),
    ToggleLaunchpad,
    OpenTrash,
    OpenSettings,
    SetHide(config::HideMode),
    ToggleMagnification,
    SetEdge(config::Edge),
    /// Novas preferências; `true` = gravar em disco (falso durante o arraste de um slider).
    ApplySettings(config::Settings, bool),
    /// Índice em `ctx.themes`.
    SetTheme(usize),
    OpenThemeFolder,
}

pub enum Msg {
    X(Event),
    Icon(String, icons::Icon),
}

/// Estado compartilhado entre a dock e o Launchpad.
pub struct Ctx {
    pub conn: Arc<RustConnection>,
    pub atoms: x::Atoms,
    pub root: Window,
    pub visual: Visualid,
    pub colormap: Colormap,
    pub mon: (i32, i32, i32, i32),
    pub apps: Vec<apps::App>,
    matcher: HashMap<String, usize>,
    synthetic: HashMap<String, usize>,
    pub icons: HashMap<String, icons::Icon>,
    pub scaled: draw::ScaleCache,
    pub text: draw::Text,
    win_cache: HashMap<Window, Option<usize>>,
    /// App → janelas, em ordem de empilhamento (a última é a do topo).
    pub running: HashMap<usize, Vec<Window>>,
    pub pinned: HashSet<usize>,
    /// Janela ativa e seu retângulo na tela (com moldura), para a ocultação inteligente.
    active_win: Window,
    pub active_rect: Option<(i32, i32, i32, i32)>,
    pub settings: config::Settings,
    /// Tema escolhido e os temas disponíveis (relidos ao abrir as Preferências).
    pub theme: String,
    pub themes: Vec<config::Theme>,
    pub own: Vec<Window>,
    /// Último timestamp X recebido (usado ao ativar janelas).
    pub time: u32,
    pub buf: Vec<u8>,
}

impl Ctx {
    pub fn theme_path(&self) -> Option<std::path::PathBuf> {
        self.themes.iter().find(|t| t.name == self.theme).and_then(|t| t.path.clone())
    }

    /// Relê a lista de janelas do gerenciador de janelas e as associa a apps.
    fn scan_clients(&mut self) {
        let conn = self.conn.clone();
        let a = self.atoms;
        let mut list = x::prop32(&conn, self.root, a._NET_CLIENT_LIST_STACKING, AtomEnum::WINDOW);
        if list.is_empty() {
            list = x::prop32(&conn, self.root, a._NET_CLIENT_LIST, AtomEnum::WINDOW);
        }
        let new: Vec<Window> = list.iter().copied().filter(|w| !self.win_cache.contains_key(w)).collect();
        let cookies: Vec<_> = new
            .iter()
            .map(|&w| {
                (
                    w,
                    conn.get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256).ok(),
                    conn.get_property(false, w, a._NET_WM_WINDOW_TYPE, AtomEnum::ATOM, 0, 32).ok(),
                    conn.get_property(false, w, a._NET_WM_STATE, AtomEnum::ATOM, 0, 64).ok(),
                )
            })
            .collect();
        for (w, class, ty, state) in cookies {
            let class = class.and_then(|c| c.reply().ok()).map(|r| r.value).unwrap_or_default();
            let ty: Vec<u32> = ty.and_then(|c| c.reply().ok()).and_then(|r| r.value32().map(|v| v.collect())).unwrap_or_default();
            let state: Vec<u32> = state.and_then(|c| c.reply().ok()).and_then(|r| r.value32().map(|v| v.collect())).unwrap_or_default();
            let skip = self.own.contains(&w)
                || ty.contains(&a._NET_WM_WINDOW_TYPE_DOCK)
                || ty.contains(&a._NET_WM_WINDOW_TYPE_DESKTOP)
                || state.contains(&a._NET_WM_STATE_SKIP_TASKBAR);
            let app = if skip || class.is_empty() { None } else { Some(self.match_window(w, &class)) };
            self.win_cache.insert(w, app);
        }
        let alive: HashSet<Window> = list.iter().copied().collect();
        self.win_cache.retain(|w, _| alive.contains(w));
        self.running.clear();
        for w in list {
            if let Some(Some(app)) = self.win_cache.get(&w) {
                self.running.entry(*app).or_default().push(w);
            }
        }
    }

    /// Relê a janela ativa e onde ela está. Passa a ouvir os movimentos dela (ConfigureNotify).
    fn update_active(&mut self) {
        let conn = self.conn.clone();
        let a = self.atoms;
        let win = x::prop32(&conn, self.root, a._NET_ACTIVE_WINDOW, AtomEnum::WINDOW).first().copied().unwrap_or(0);
        // Não mexer na máscara das nossas próprias janelas: para o mesmo cliente ela é substituída,
        // e a janela de Preferências perderia cliques, movimento e redesenho.
        if win != self.active_win && win != x11rb::NONE && !self.own.contains(&win) {
            let aux = ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE);
            let _ = conn.change_window_attributes(win, &aux);
        }
        self.active_win = win;
        self.active_rect = self.window_rect(win);
    }

    fn window_rect(&self, win: Window) -> Option<(i32, i32, i32, i32)> {
        let conn = &self.conn;
        let a = &self.atoms;
        if win == x11rb::NONE || self.own.contains(&win) {
            return None;
        }
        let ty = x::prop32(conn, win, a._NET_WM_WINDOW_TYPE, AtomEnum::ATOM);
        let state = x::prop32(conn, win, a._NET_WM_STATE, AtomEnum::ATOM);
        if ty.contains(&a._NET_WM_WINDOW_TYPE_DESKTOP) || ty.contains(&a._NET_WM_WINDOW_TYPE_DOCK) || state.contains(&a._NET_WM_STATE_HIDDEN) {
            return None;
        }
        let attrs = conn.get_window_attributes(win).ok()?.reply().ok()?;
        if attrs.map_state != MapState::VIEWABLE {
            return None;
        }
        let g = conn.get_geometry(win).ok()?.reply().ok()?;
        let t = conn.translate_coordinates(win, self.root, 0, 0).ok()?.reply().ok()?;
        let f = x::prop32(conn, win, a._NET_FRAME_EXTENTS, AtomEnum::CARDINAL);
        let (l, r, top, b) = if f.len() >= 4 { (f[0] as i32, f[1] as i32, f[2] as i32, f[3] as i32) } else { (0, 0, 0, 0) };
        Some((t.dst_x as i32 - l, t.dst_y as i32 - top, g.width as i32 + l + r, g.height as i32 + top + b))
    }

    fn match_window(&mut self, win: Window, wm_class: &[u8]) -> usize {
        let s = String::from_utf8_lossy(wm_class);
        let mut parts = s.split('\0');
        let instance = parts.next().unwrap_or("").to_lowercase();
        let class = parts.next().unwrap_or("").to_lowercase();
        for key in [&class, &instance, &class.replace(' ', "-")] {
            if let Some(&i) = self.matcher.get(key.as_str()) {
                return i;
            }
        }
        let key = if class.is_empty() { instance } else { class };
        if let Some(&i) = self.synthetic.get(&key) {
            return i;
        }
        let icon_key = format!("wm:{key}");
        let pix = x::wm_icon(&self.conn, &self.atoms, win).unwrap_or_else(icons::generic_icon);
        self.icons.insert(icon_key.clone(), icons::Icon::new(pix));
        let original = String::from_utf8_lossy(wm_class).split('\0').nth(1).unwrap_or(&key).to_string();
        self.apps.push(apps::App::synthetic(&original, icon_key));
        let i = self.apps.len() - 1;
        self.synthetic.insert(key, i);
        i
    }
}

fn main() -> R<()> {
    let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("não foi possível conectar ao X11: {e}"))?;
    let conn = Arc::new(conn);
    let screen = conn.setup().roots[screen_num].clone();
    let atoms = x::Atoms::new(&*conn)?.reply()?;

    let open_settings = std::env::args().any(|a| a == "--settings");
    let owner = conn.get_selection_owner(atoms._RDOCK_RUNNING)?.reply()?.owner;
    if owner != x11rb::NONE {
        if open_settings {
            // Pede à dock que já está rodando para abrir as preferências.
            let ev = ClientMessageEvent::new(32, owner, atoms._RDOCK_SETTINGS, [0u32; 5]);
            conn.send_event(false, owner, EventMask::NO_EVENT, ev)?.check()?;
        } else {
            eprintln!("rdock já está em execução.");
        }
        return Ok(());
    }
    let visual = x::argb_visual(&screen).ok_or("o servidor X não tem visual ARGB de 32 bits (compositor necessário)")?;
    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)?;
    conn.change_window_attributes(screen.root, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE))?;

    let apps = apps::scan();
    let mut ctx = Ctx {
        conn: conn.clone(),
        atoms,
        root: screen.root,
        visual,
        colormap,
        mon: x::primary_monitor(&conn, &screen),
        matcher: apps::build_matcher(&apps),
        apps,
        synthetic: HashMap::new(),
        icons: HashMap::new(),
        scaled: draw::ScaleCache::default(),
        text: draw::Text::load(),
        win_cache: HashMap::new(),
        running: HashMap::new(),
        pinned: HashSet::new(),
        active_win: x11rb::NONE,
        active_rect: None,
        settings: config::load_settings(),
        theme: config::load_theme_name(),
        themes: config::list_themes(),
        own: Vec::new(),
        time: x11rb::CURRENT_TIME,
        buf: Vec::new(),
    };
    ctx.text.scale = ctx.settings.text_scale;
    let mut dock = dock::Dock::new(&mut ctx)?;
    let mut lp = launchpad::Launchpad::new(&mut ctx)?;
    let mut sw = settings::SettingsWin::new(&mut ctx)?;
    // O dono da seleção é a janela do Launchpad, que nunca é recriada (a da dock é, ao mudar de borda).
    conn.set_selection_owner(lp.win, atoms._RDOCK_RUNNING, x11rb::CURRENT_TIME)?;
    if open_settings {
        sw.open(&mut ctx)?;
    }

    let (tx, rx) = mpsc::channel();
    let mut names = vec!["@launchpad".to_string()];
    names.extend(dock.icon_names(&ctx));
    names.extend(ctx.apps.iter().map(|a| a.icon.clone()));
    let mut seen = HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    icons::spawn_loader(names, tx.clone());

    let econn = conn.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = econn.wait_for_event() {
            if tx.send(Msg::X(ev)).is_err() {
                break;
            }
        }
    });

    ctx.scan_clients();
    ctx.update_active();
    dock.sync_running(&ctx);
    conn.flush()?;

    const FRAME: Duration = Duration::from_micros(16_667);
    let mut last_frame = Instant::now();
    let mut last_trash = Instant::now();
    loop {
        let animating = dock.needs_frame || lp.needs_frame || sw.needs_frame;
        let timeout = if animating { FRAME.saturating_sub(last_frame.elapsed()) } else { Duration::from_secs(2) };
        let mut msgs = Vec::new();
        match rx.recv_timeout(timeout) {
            Ok(m) => msgs.push(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        msgs.extend(rx.try_iter());
        for m in msgs {
            for act in handle(&mut ctx, &mut dock, &mut lp, &mut sw, m) {
                dispatch(&mut ctx, &mut dock, &mut lp, &mut sw, act)?;
            }
        }
        if last_trash.elapsed() > Duration::from_secs(2) {
            last_trash = Instant::now();
            dock.set_trash_full(dock::trash_is_full());
        }
        if (dock.needs_frame || lp.needs_frame || sw.needs_frame) && last_frame.elapsed() >= FRAME.mul_f32(0.9) {
            let dt = last_frame.elapsed().as_secs_f32().min(1.0 / 30.0);
            last_frame = Instant::now();
            if dock.needs_frame {
                dock.needs_frame = dock.tick(&mut ctx, dt);
                dock.render(&mut ctx)?;
            }
            if lp.needs_frame {
                lp.needs_frame = lp.tick(&mut ctx, dt);
                lp.render(&mut ctx)?;
            }
            if sw.needs_frame {
                sw.needs_frame = sw.tick(&ctx, dt);
                sw.render(&mut ctx)?;
            }
        } else if !(dock.needs_frame || lp.needs_frame || sw.needs_frame) {
            // Ocioso: o próximo quadro começa com dt de um quadro, não do tempo parado.
            last_frame = Instant::now() - FRAME;
        }
        conn.flush()?;
    }
    Ok(())
}

fn handle(ctx: &mut Ctx, dock: &mut dock::Dock, lp: &mut launchpad::Launchpad, sw: &mut settings::SettingsWin, msg: Msg) -> Vec<Action> {
    let ev = match msg {
        Msg::Icon(name, icon) => {
            ctx.icons.insert(name, icon);
            dock.needs_frame = true;
            lp.damage();
            return vec![];
        }
        Msg::X(ev) => ev,
    };
    let mut out = Vec::new();
    match ev {
        Event::MotionNotify(e) => {
            ctx.time = e.time;
            let (x, y) = (e.event_x as f32, e.event_y as f32);
            if e.event == dock.win {
                dock.on_motion(ctx, x, y);
            } else if lp.is_window(e.event) {
                lp.on_motion(x, y);
            } else if e.event == sw.win {
                out.extend(sw.on_motion(ctx, x, y));
            }
        }
        Event::EnterNotify(e) if e.event == dock.win => {
            ctx.time = e.time;
            dock.on_motion(ctx, e.event_x as f32, e.event_y as f32);
        }
        Event::LeaveNotify(e) if e.event == dock.win && e.mode == NotifyMode::NORMAL => dock.on_leave(),
        Event::ButtonPress(e) => {
            ctx.time = e.time;
            let (x, y) = (e.event_x as f32, e.event_y as f32);
            if e.event == dock.win {
                dock.on_press(ctx, e.detail, x, y);
            } else if lp.is_window(e.event) {
                lp.on_press(ctx, e.detail, x, y);
            } else if e.event == sw.win {
                out.extend(sw.on_press(ctx, e.detail, x, y));
            }
        }
        Event::ButtonRelease(e) => {
            ctx.time = e.time;
            let (x, y) = (e.event_x as f32, e.event_y as f32);
            let act = if e.event == dock.win {
                dock.on_release(ctx, e.detail, x, y)
            } else if lp.is_window(e.event) {
                lp.on_release(ctx, e.detail, x, y)
            } else if e.event == sw.win {
                sw.on_release(ctx)
            } else {
                None
            };
            out.extend(act);
        }
        Event::KeyPress(e) if lp.visible => {
            ctx.time = e.time;
            out.extend(lp.on_key(ctx, e.detail, e.state));
        }
        Event::PropertyNotify(e) if e.window == ctx.root => {
            let a = ctx.atoms;
            if e.atom == a._NET_CLIENT_LIST || e.atom == a._NET_CLIENT_LIST_STACKING {
                ctx.scan_clients();
                dock.sync_running(ctx);
            }
            if e.atom == a._NET_ACTIVE_WINDOW || e.atom == a._NET_CLIENT_LIST_STACKING {
                ctx.update_active();
                dock.needs_frame = true;
            }
        }
        // A janela ativa mudou de lugar, foi minimizada ou restaurada.
        Event::PropertyNotify(e) if e.window == ctx.active_win && e.atom == ctx.atoms._NET_WM_STATE => {
            ctx.update_active();
            dock.needs_frame = true;
        }
        Event::ConfigureNotify(e) if e.window == ctx.active_win => {
            ctx.update_active();
            dock.needs_frame = true;
        }
        Event::UnmapNotify(e) if e.window == ctx.active_win => {
            ctx.update_active();
            dock.needs_frame = true;
        }
        Event::MapNotify(e) if e.window == ctx.active_win => {
            ctx.update_active();
            dock.needs_frame = true;
        }
        Event::Expose(e) if e.count == 0 => {
            if e.window == dock.win {
                dock.damage();
            } else if lp.is_window(e.window) {
                lp.damage();
            } else if e.window == sw.win {
                sw.damage();
            }
        }
        Event::ClientMessage(e) => {
            let a = ctx.atoms;
            if e.type_ == a._RDOCK_SETTINGS {
                out.push(Action::OpenSettings);
            } else if e.window == sw.win && e.type_ == a.WM_PROTOCOLS && e.data.as_data32()[0] == a.WM_DELETE_WINDOW {
                sw.close(ctx);
            }
        }
        _ => {}
    }
    out
}

fn dispatch(ctx: &mut Ctx, dock: &mut dock::Dock, lp: &mut launchpad::Launchpad, sw: &mut settings::SettingsWin, act: Action) -> R<()> {
    let mut apply = |ctx: &mut Ctx, s: config::Settings, persist: bool| -> R<()> {
        ctx.settings = s;
        ctx.text.scale = s.text_scale;
        lp.text_scale = s.text_scale;
        dock.apply(ctx)?;
        dock.damage();
        sw.damage();
        if persist {
            config::save_settings(&s);
        }
        Ok(())
    };
    match act {
        Action::Launch(app) => {
            dock.launch(ctx, app);
            lp.close(ctx);
        }
        Action::TogglePin(app) => dock.toggle_pin(ctx, app),
        Action::Quit(app) => {
            for w in ctx.running.get(&app).cloned().unwrap_or_default() {
                x::close(&ctx.conn, &ctx.atoms, ctx.root, w, ctx.time);
            }
        }
        Action::ToggleLaunchpad => {
            if lp.visible && !lp.closing {
                lp.close(ctx);
            } else {
                lp.open(ctx)?;
            }
        }
        Action::OpenTrash => apps::open_uri("trash:///"),
        Action::SetTheme(i) => {
            if let Some(t) = ctx.themes.get(i) {
                ctx.theme = t.name.clone();
                config::save_theme_name(&ctx.theme);
                dock.reload_theme(ctx);
                sw.damage();
            }
        }
        Action::OpenThemeFolder => {
            let dir = config::user_theme_dir();
            let _ = std::fs::create_dir_all(&dir);
            apps::open_uri(&dir.to_string_lossy());
        }
        Action::OpenSettings => {
            ctx.themes = config::list_themes();
            lp.close(ctx);
            sw.open(ctx)?;
        }
        Action::SetHide(hide) => apply(ctx, config::Settings { hide, ..ctx.settings }, true)?,
        Action::ToggleMagnification => {
            apply(ctx, config::Settings { magnification: !ctx.settings.magnification, ..ctx.settings }, true)?
        }
        Action::SetEdge(edge) => apply(ctx, config::Settings { edge, ..ctx.settings }, true)?,
        Action::ApplySettings(s, persist) => apply(ctx, s, persist)?,
    }
    Ok(())
}
