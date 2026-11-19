//! rdock — uma dock no estilo do macOS para X11 (Cinnamon, XFCE, MATE, i3…), em Rust.

mod anim;
mod apps;
mod config;
mod dock;
mod draw;
mod files;
mod icons;
mod launchpad;
mod menu;
mod previews;
mod settings;
mod stack;
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
    /// Arrastar um app do Launchpad para a dock: app e posição do ponteiro na tela.
    DragToDock(usize, f32, f32),
    OpenThemeFolder,
    /// Gavetas (índice em `ctx.stacks`).
    OpenStack(usize),
    OpenStackFolder(usize),
    SetStackStyle(usize, stack::Style),
    SetStackSort(usize, stack::Sort),
    RemoveStack(usize),
    /// Arquivo recente `k` (em `ctx.recent`) com o app.
    OpenRecent(usize, usize),
    NewWindow(usize),
}

pub enum Msg {
    X(Event),
    Icon(String, icons::Icon),
    /// Uma pasta observada (inotify, pelo descritor da observação) mudou.
    Dir(i32),
    /// Miniatura de uma janela para a prévia.
    Thumb(Window, tiny_skia::Pixmap),
}

/// As janelas da interface.
struct Ui {
    dock: dock::Dock,
    lp: launchpad::Launchpad,
    sw: settings::SettingsWin,
    sv: stack::StackView,
    pv: previews::Previews,
}

impl Ui {
    fn busy(&self) -> bool {
        self.dock.needs_frame || self.lp.needs_frame || self.sw.needs_frame || self.sv.needs_frame || self.pv.needs_frame
    }
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
    /// Canal para pedir o carregamento de ícones depois da inicialização.
    icon_tx: mpsc::Sender<Msg>,
    icons_requested: HashSet<String>,
    /// Último timestamp X recebido (usado ao ativar janelas).
    pub time: u32,
    pub stacks: Vec<stack::Stack>,
    watcher: Option<stack::Watcher>,
    /// Arquivos recentes mostrados no último menu de app.
    pub recent: Vec<std::path::PathBuf>,
    /// "Reduzir movimento": as molas viram só fade.
    pub reduce_motion: bool,
}

impl Ctx {
    /// App de um arquivo .desktop: pelo caminho, pelo ID, ou carregando o arquivo como novo app.
    pub fn app_for_file(&mut self, path: &std::path::Path) -> Option<usize> {
        if let Some(i) = self.apps.iter().position(|a| a.path == path) {
            return Some(i);
        }
        let name = path.file_name()?.to_string_lossy().to_string();
        if let Some(i) = self.apps.iter().position(|a| a.id == name && a.listed) {
            return Some(i);
        }
        self.add_app(apps::load_file(path)?)
    }

    fn add_app(&mut self, app: apps::App) -> Option<usize> {
        self.ensure_icon(app.icon.clone());
        self.apps.push(app);
        self.matcher = apps::build_matcher(&self.apps);
        Some(self.apps.len() - 1)
    }

    pub fn ensure_icon(&mut self, name: String) {
        self.ensure_icons(vec![name]);
    }

    /// Pede de uma vez os ícones que ainda faltam (um único carregador para o lote).
    pub fn ensure_icons(&mut self, names: Vec<String>) {
        let missing: Vec<String> = names.into_iter().filter(|n| !self.icons.contains_key(n) && self.icons_requested.insert(n.clone())).collect();
        if !missing.is_empty() {
            icons::spawn_loader(missing, self.icon_tx.clone());
        }
    }

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

    apps::install_launcher();
    let apps = apps::scan();
    let (tx, rx) = mpsc::channel();
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
        icon_tx: tx.clone(),
        icons_requested: HashSet::new(),
        time: x11rb::CURRENT_TIME,
        stacks: stack::load(),
        watcher: stack::Watcher::new(tx.clone()),
        recent: Vec::new(),
        reduce_motion: reduce_motion(),
    };
    ctx.text.scale = ctx.settings.text_scale;
    if let Some(w) = &ctx.watcher {
        ctx.stacks.iter_mut().for_each(|s| w.watch(s));
    }
    let mut ui = Ui {
        dock: dock::Dock::new(&mut ctx)?,
        lp: launchpad::Launchpad::new(&mut ctx)?,
        sw: settings::SettingsWin::new(&mut ctx)?,
        sv: stack::StackView::new(&mut ctx)?,
        pv: previews::Previews::new(&mut ctx, tx.clone())?,
    };
    // O dono da seleção é a janela do Launchpad, que nunca é recriada (a da dock é, ao mudar de borda).
    conn.set_selection_owner(ui.lp.win, atoms._RDOCK_RUNNING, x11rb::CURRENT_TIME)?;
    if open_settings {
        ui.sw.open(&mut ctx)?;
    }

    let mut names = vec!["@launchpad".to_string()];
    names.extend(ui.dock.icon_names(&ctx));
    names.extend(ctx.apps.iter().map(|a| a.icon.clone()));
    let mut seen = HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    ctx.icons_requested.extend(names.iter().cloned());
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
    ui.dock.sync_running(&ctx);
    conn.flush()?;
    const PREVIEW_DELAY: Duration = Duration::from_millis(400);

    const FRAME: Duration = Duration::from_micros(16_667);
    let mut last_frame = Instant::now();
    let mut last_trash = Instant::now();
    loop {
        // Mouse parado sobre um app: acorda a tempo de abrir a prévia de janelas.
        let waiting = ctx.settings.previews && ui.dock.hover_app().is_some() && !ui.pv.visible;
        let timeout = if ui.busy() {
            FRAME.saturating_sub(last_frame.elapsed())
        } else if waiting {
            Duration::from_millis(60)
        } else {
            Duration::from_secs(2)
        };
        let mut msgs = Vec::new();
        match rx.recv_timeout(timeout) {
            Ok(m) => msgs.push(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        msgs.extend(rx.try_iter());
        for m in msgs {
            for act in handle(&mut ctx, &mut ui, m) {
                dispatch(&mut ctx, &mut ui, act)?;
            }
        }
        // Pasta solta na dock: vira gaveta.
        if let Some(path) = ui.dock.pending_stack.take() {
            let mut st = stack::Stack::new(path, stack::Style::Auto, stack::Sort::Recent, stack::now());
            if let Some(w) = &ctx.watcher {
                w.watch(&mut st);
            }
            ctx.ensure_icon(st.icon.clone());
            ctx.stacks.push(st);
            stack::save(&ctx.stacks);
            ui.dock.add_stack(&ctx, ctx.stacks.len() - 1);
        }
        // Prévia de janelas
        if ctx.settings.previews && !ui.sv.visible && !ui.lp.visible {
            if let Some((app, anchor)) = ui.dock.hovered_app(PREVIEW_DELAY) {
                let has_windows = ctx.running.get(&app).is_some_and(|w| !w.is_empty());
                if has_windows && (!ui.pv.visible || ui.pv.closing || ui.pv.app != app) {
                    ui.pv.open(&mut ctx, app, anchor)?;
                }
            }
        }
        ui.pv.update(&ctx, ui.dock.hover_app());
        let open = ui.pv.visible && !ui.pv.closing;
        if open != ui.dock.preview_open {
            ui.dock.preview_open = open;
            ui.dock.needs_frame = true;
        }
        if last_trash.elapsed() > Duration::from_secs(2) {
            last_trash = Instant::now();
            ui.dock.set_trash_full(dock::trash_is_full());
        }
        if ui.busy() && last_frame.elapsed() >= FRAME.mul_f32(0.9) {
            let dt = last_frame.elapsed().as_secs_f32().min(1.0 / 30.0);
            last_frame = Instant::now();
            if ui.dock.needs_frame {
                ui.dock.needs_frame = ui.dock.tick(&mut ctx, dt);
                ui.dock.render(&mut ctx)?;
            }
            if ui.lp.needs_frame {
                ui.lp.needs_frame = ui.lp.tick(&mut ctx, dt);
                ui.lp.render(&mut ctx)?;
            }
            if ui.sw.needs_frame {
                ui.sw.needs_frame = ui.sw.tick(&ctx, dt);
                ui.sw.render(&mut ctx)?;
            }
            if ui.sv.needs_frame {
                ui.sv.needs_frame = ui.sv.tick(&mut ctx, dt);
                ui.sv.render(&mut ctx)?;
            }
            if ui.pv.needs_frame {
                ui.pv.needs_frame = ui.pv.tick(&ctx, dt);
                ui.pv.render(&mut ctx)?;
            }
        } else if !ui.busy() {
            // Ocioso: o próximo quadro começa com dt de um quadro, não do tempo parado.
            last_frame = Instant::now() - FRAME;
        }
        conn.flush()?;
    }
    Ok(())
}

fn handle(ctx: &mut Ctx, ui: &mut Ui, msg: Msg) -> Vec<Action> {
    let ev = match msg {
        Msg::Icon(name, icon) => {
            ctx.icons.insert(name, icon);
            ui.dock.needs_frame = true;
            ui.lp.damage();
            ui.sv.needs_frame |= ui.sv.visible;
            ui.pv.needs_frame |= ui.pv.visible;
            return vec![];
        }
        Msg::Dir(wd) => {
            if let Some(i) = stack::by_wd(&ctx.stacks, wd) {
                ctx.stacks[i].refresh();
                ui.dock.needs_frame = true;
                ui.sv.changed(ctx, i);
            }
            return vec![];
        }
        Msg::Thumb(w, pix) => {
            ui.pv.on_thumb(w, pix);
            return vec![];
        }
        Msg::X(ev) => ev,
    };
    let mut out = Vec::new();
    match ev {
        Event::MotionNotify(e) => {
            ctx.time = e.time;
            let (x, y) = (e.event_x as f32, e.event_y as f32);
            if e.event == ui.dock.win {
                ui.dock.on_motion(ctx, x, y);
            } else if e.event == ui.lp.win {
                out.extend(ui.lp.on_motion(x, y));
            } else if e.event == ui.sw.win {
                out.extend(ui.sw.on_motion(ctx, x, y));
            } else if e.event == ui.sv.win {
                ui.sv.on_motion(ctx, x, y);
            } else if e.event == ui.pv.win {
                ui.pv.on_motion(ctx, x, y);
            }
        }
        Event::EnterNotify(e) if e.event == ui.pv.win => ui.pv.on_crossing(true),
        Event::LeaveNotify(e) if e.event == ui.pv.win && e.detail != NotifyDetail::INFERIOR => ui.pv.on_crossing(false),
        Event::EnterNotify(e) if e.event == ui.dock.win => {
            ctx.time = e.time;
            ui.dock.on_motion(ctx, e.event_x as f32, e.event_y as f32);
        }
        Event::LeaveNotify(e) if e.event == ui.dock.win && e.mode == NotifyMode::NORMAL => ui.dock.on_leave(),
        Event::ButtonPress(e) => {
            ctx.time = e.time;
            let (x, y) = (e.event_x as f32, e.event_y as f32);
            if e.event == ui.dock.win {
                ui.dock.on_press(ctx, e.detail, x, y);
            } else if e.event == ui.lp.win {
                ui.lp.on_press(ctx, e.detail, x, y);
            } else if e.event == ui.sw.win {
                out.extend(ui.sw.on_press(ctx, e.detail, x, y));
            } else if e.event == ui.sv.win {
                ui.sv.on_press(ctx, e.detail, x, y);
            }
        }
        Event::ButtonRelease(e) => {
            ctx.time = e.time;
            let (x, y) = (e.event_x as f32, e.event_y as f32);
            let act = if e.event == ui.dock.win {
                ui.dock.on_release(ctx, e.detail, x, y)
            } else if e.event == ui.lp.win {
                ui.lp.on_release(ctx, e.detail, x, y)
            } else if e.event == ui.sw.win {
                ui.sw.on_release(ctx)
            } else if e.event == ui.pv.win {
                ui.pv.on_release(ctx, e.detail, x, y);
                None
            } else {
                None
            };
            out.extend(act);
        }
        Event::KeyPress(e) if ui.sv.visible => {
            ctx.time = e.time;
            ui.sv.on_key(ctx, e.detail, e.state);
        }
        Event::KeyPress(e) if ui.lp.visible => {
            ctx.time = e.time;
            out.extend(ui.lp.on_key(ctx, e.detail, e.state));
        }
        Event::PropertyNotify(e) if e.window == ctx.root => {
            let a = ctx.atoms;
            if e.atom == a._NET_CLIENT_LIST || e.atom == a._NET_CLIENT_LIST_STACKING {
                ctx.scan_clients();
                ui.dock.sync_running(ctx);
            }
            if e.atom == a._NET_ACTIVE_WINDOW || e.atom == a._NET_CLIENT_LIST_STACKING {
                ctx.update_active();
                ui.dock.needs_frame = true;
            }
        }
        // A janela ativa mudou de lugar, foi minimizada ou restaurada.
        Event::PropertyNotify(e) if e.window == ctx.active_win && e.atom == ctx.atoms._NET_WM_STATE => {
            ctx.update_active();
            ui.dock.needs_frame = true;
        }
        Event::ConfigureNotify(e) if e.window == ctx.active_win => {
            ctx.update_active();
            ui.dock.needs_frame = true;
        }
        Event::UnmapNotify(e) if e.window == ctx.active_win => {
            ctx.update_active();
            ui.dock.needs_frame = true;
        }
        Event::MapNotify(e) if e.window == ctx.active_win => {
            ctx.update_active();
            ui.dock.needs_frame = true;
        }
        Event::Expose(e) if e.count == 0 => {
            if e.window == ui.dock.win {
                ui.dock.damage();
            } else if e.window == ui.lp.win {
                ui.lp.damage();
            } else if e.window == ui.sw.win {
                ui.sw.damage();
            } else if e.window == ui.sv.win {
                ui.sv.needs_frame = true;
            } else if e.window == ui.pv.win {
                ui.pv.needs_frame = true;
            }
        }
        Event::SelectionNotify(e) if e.requestor == ui.dock.win => ui.dock.on_xdnd_data(ctx, e.property),
        Event::ClientMessage(e) => {
            let a = ctx.atoms;
            if e.window == ui.dock.win && [a.XdndEnter, a.XdndPosition, a.XdndLeave, a.XdndDrop].contains(&e.type_) {
                ui.dock.on_xdnd(ctx, &e);
            } else if e.type_ == a._RDOCK_SETTINGS {
                out.push(Action::OpenSettings);
            } else if e.window == ui.sw.win && e.type_ == a.WM_PROTOCOLS && e.data.as_data32()[0] == a.WM_DELETE_WINDOW {
                ui.sw.close(ctx);
            }
        }
        _ => {}
    }
    out
}

fn apply(ctx: &mut Ctx, ui: &mut Ui, s: config::Settings, persist: bool) -> R<()> {
    ctx.settings = s;
    ctx.text.scale = s.text_scale;
    ui.dock.apply(ctx)?;
    ui.dock.damage();
    ui.sw.damage();
    if persist {
        config::save_settings(&s);
    }
    Ok(())
}

fn dispatch(ctx: &mut Ctx, ui: &mut Ui, act: Action) -> R<()> {
    match act {
        Action::Launch(app) => {
            ui.dock.launch(ctx, app);
            ui.lp.close(ctx);
        }
        Action::TogglePin(app) => ui.dock.toggle_pin(ctx, app),
        Action::Quit(app) => {
            for w in ctx.running.get(&app).cloned().unwrap_or_default() {
                x::close(&ctx.conn, &ctx.atoms, ctx.root, w, ctx.time);
            }
        }
        Action::ToggleLaunchpad => {
            if ui.lp.visible && !ui.lp.closing {
                ui.lp.close(ctx);
            } else {
                ui.lp.open(ctx)?;
            }
        }
        Action::OpenTrash => apps::open_uri("trash:///"),
        Action::DragToDock(app, x, y) => {
            ui.lp.close(ctx);
            ui.dock.begin_external_drag(ctx, app, x, y);
        }
        Action::SetTheme(i) => {
            if let Some(t) = ctx.themes.get(i) {
                ctx.theme = t.name.clone();
                config::save_theme_name(&ctx.theme);
                ui.dock.reload_theme(ctx);
                ui.sw.damage();
            }
        }
        Action::OpenThemeFolder => {
            let dir = config::user_theme_dir();
            let _ = std::fs::create_dir_all(&dir);
            apps::open_uri(&dir.to_string_lossy());
        }
        Action::OpenSettings => {
            ctx.themes = config::list_themes();
            ui.lp.close(ctx);
            ui.sw.open(ctx)?;
        }
        Action::SetHide(hide) => apply(ctx, ui, config::Settings { hide, ..ctx.settings }, true)?,
        Action::ToggleMagnification => {
            apply(ctx, ui, config::Settings { magnification: !ctx.settings.magnification, ..ctx.settings }, true)?
        }
        Action::SetEdge(edge) => apply(ctx, ui, config::Settings { edge, ..ctx.settings }, true)?,
        Action::ApplySettings(s, persist) => apply(ctx, ui, s, persist)?,
        Action::OpenStack(i) => {
            if ui.sv.visible && !ui.sv.closing_now() && ui.sv.stack == i {
                ui.sv.close(ctx);
            } else if let Some(anchor) = ui.dock.stack_anchor(i) {
                ui.pv.close();
                ui.lp.close(ctx);
                ui.sv.open(ctx, i, anchor)?;
                ui.dock.release_mouse();
            }
        }
        Action::OpenStackFolder(i) => {
            if let Some(st) = ctx.stacks.get(i) {
                files::open(&st.path);
            }
        }
        Action::SetStackStyle(i, style) => {
            if let Some(st) = ctx.stacks.get_mut(i) {
                st.style = style;
                stack::save(&ctx.stacks);
            }
        }
        Action::SetStackSort(i, sort) => {
            if let Some(st) = ctx.stacks.get_mut(i) {
                st.sort = sort;
                stack::save(&ctx.stacks);
            }
        }
        Action::RemoveStack(i) => {
            if i < ctx.stacks.len() {
                ui.sv.close(ctx);
                if let Some(w) = &ctx.watcher {
                    w.unwatch(&ctx.stacks[i]);
                }
                ctx.stacks.remove(i);
                stack::save(&ctx.stacks);
                ui.dock.remove_stack(i);
            }
        }
        Action::OpenRecent(app, k) => {
            if let Some(p) = ctx.recent.get(k).cloned() {
                files::open_with(&ctx.apps[app], &[p]);
            }
        }
        Action::NewWindow(app) => {
            apps::launch(&ctx.apps[app]);
            ui.dock.launch_feedback(app);
        }
    }
    Ok(())
}

/// "Reduzir movimento" do sistema (animações desligadas no GNOME/Cinnamon).
fn reduce_motion() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "false")
}
