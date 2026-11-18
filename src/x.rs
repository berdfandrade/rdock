//! Utilitários X11: janelas ARGB, upload de imagem, propriedades EWMH e controle de janelas.

use tiny_skia::Pixmap;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

use crate::R;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        UTF8_STRING,
        _NET_WM_NAME,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_STATE,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_STATE_SKIP_PAGER,
        _NET_WM_STATE_STICKY,
        _NET_WM_STATE_ABOVE,
        _NET_WM_DESKTOP,
        _NET_WM_STRUT,
        _NET_WM_STRUT_PARTIAL,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_ACTIVE_WINDOW,
        _NET_CLOSE_WINDOW,
        _NET_WM_ICON,
        _NET_WORKAREA,
        _NET_WM_WINDOW_TYPE_DIALOG,
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        _RDOCK_RUNNING,
        _RDOCK_SETTINGS,
    }
}

pub fn argb_visual(screen: &Screen) -> Option<Visualid> {
    screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| &d.visuals)
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id)
}

pub fn primary_monitor(conn: &RustConnection, screen: &Screen) -> (i32, i32, i32, i32) {
    let fallback = (0, 0, screen.width_in_pixels as i32, screen.height_in_pixels as i32);
    let _ = conn.randr_query_version(1, 5).map(|c| c.reply());
    let Ok(Ok(r)) = conn.randr_get_monitors(screen.root, true).map(|c| c.reply()) else { return fallback };
    r.monitors
        .iter()
        .find(|m| m.primary)
        .or(r.monitors.first())
        .map(|m| (m.x as i32, m.y as i32, m.width as i32, m.height as i32))
        .unwrap_or(fallback)
}

pub fn create_window(
    conn: &RustConnection,
    root: Window,
    visual: Visualid,
    colormap: Colormap,
    geom: (i32, i32, i32, i32),
    override_redirect: bool,
    events: EventMask,
) -> R<(Window, Gcontext)> {
    let win = conn.generate_id()?;
    let (x, y, w, h) = geom;
    conn.create_window(
        32,
        win,
        root,
        x as i16,
        y as i16,
        w as u16,
        h as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            .override_redirect(u32::from(override_redirect))
            .event_mask(events),
    )?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new())?;
    Ok((win, gc))
}

/// Envia a região (x,y,w,h) do pixmap RGBA pré-multiplicado para a janela ARGB (BGRA).
pub fn put_region(
    conn: &RustConnection,
    win: Window,
    gc: Gcontext,
    pix: &Pixmap,
    rect: (i32, i32, i32, i32),
    buf: &mut Vec<u8>,
) -> R<()> {
    let (pw, ph) = (pix.width() as i32, pix.height() as i32);
    let x0 = rect.0.clamp(0, pw);
    let y0 = rect.1.clamp(0, ph);
    let x1 = (rect.0 + rect.2).clamp(0, pw);
    let y1 = (rect.1 + rect.3).clamp(0, ph);
    if x1 <= x0 || y1 <= y0 {
        return Ok(());
    }
    let w = (x1 - x0) as usize;
    let stride = pw as usize * 4;
    let row_bytes = w * 4;
    let max = conn.maximum_request_bytes().min(8 << 20) - 64;
    let rows_per = (max / row_bytes).max(1);
    let data = pix.data();
    let mut row = y0 as usize;
    while row < y1 as usize {
        let n = rows_per.min(y1 as usize - row);
        buf.resize(n * row_bytes, 0);
        for r in 0..n {
            let src = &data[(row + r) * stride + x0 as usize * 4..][..row_bytes];
            let dst = &mut buf[r * row_bytes..][..row_bytes];
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
        }
        conn.put_image(ImageFormat::Z_PIXMAP, win, gc, w as u16, n as u16, x0 as i16, row as i16, 0, 32, buf)?;
        row += n;
    }
    Ok(())
}

pub fn prop32(conn: &RustConnection, win: Window, prop: Atom, ty: impl Into<Atom>) -> Vec<u32> {
    conn.get_property(false, win, prop, ty, 0, u32::MAX / 4)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|v| v.collect()))
        .unwrap_or_default()
}

/// Escolhe o maior ícone de `_NET_WM_ICON` (até 256px).
pub fn wm_icon(conn: &RustConnection, atoms: &Atoms, win: Window) -> Option<Pixmap> {
    let data = prop32(conn, win, atoms._NET_WM_ICON, AtomEnum::CARDINAL);
    let mut i = 0;
    let mut best: Option<(u32, u32, usize)> = None;
    while i + 2 <= data.len() {
        let (w, h) = (data[i], data[i + 1]);
        let n = (w as usize) * (h as usize);
        if n == 0 || i + 2 + n > data.len() {
            break;
        }
        if w <= 256 && best.is_none_or(|(bw, _, _)| w > bw) {
            best = Some((w, h, i + 2));
        }
        i += 2 + n;
    }
    let (w, h, off) = best?;
    crate::icons::from_argb(w, h, &data[off..off + (w * h) as usize])
}

fn send_root_message(conn: &RustConnection, root: Window, win: Window, ty: Atom, data: [u32; 5]) {
    let ev = ClientMessageEvent::new(32, win, ty, data);
    let _ = conn.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
}

pub fn activate(conn: &RustConnection, atoms: &Atoms, root: Window, win: Window, time: u32) {
    send_root_message(conn, root, win, atoms._NET_ACTIVE_WINDOW, [2, time, 0, 0, 0]);
}

pub fn close(conn: &RustConnection, atoms: &Atoms, root: Window, win: Window, time: u32) {
    send_root_message(conn, root, win, atoms._NET_CLOSE_WINDOW, [time, 2, 0, 0, 0]);
}

/// Tabela keycode → keysym (só grupo 1, com/sem Shift), suficiente para a busca do Launchpad.
pub struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keymap {
    pub fn load(conn: &RustConnection) -> R<Keymap> {
        let s = conn.setup();
        let r = conn.get_keyboard_mapping(s.min_keycode, s.max_keycode - s.min_keycode + 1)?.reply()?;
        Ok(Keymap { min: s.min_keycode, per: r.keysyms_per_keycode as usize, syms: r.keysyms })
    }

    pub fn keysym(&self, code: u8, shift: bool) -> u32 {
        let base = code.saturating_sub(self.min) as usize * self.per;
        let get = |i: usize| if i < self.per { self.syms.get(base + i).copied().unwrap_or(0) } else { 0 };
        let (s0, s1) = (get(0), get(1));
        if !shift {
            return s0;
        }
        if s1 != 0 {
            return s1;
        }
        keysym_char(s0).map(|c| c.to_uppercase().next().unwrap_or(c) as u32).unwrap_or(s0)
    }
}

pub fn keysym_char(ks: u32) -> Option<char> {
    if (0x20..=0x7e).contains(&ks) || (0xa0..=0xff).contains(&ks) {
        char::from_u32(ks)
    } else if ks & 0xff00_0000 == 0x0100_0000 {
        char::from_u32(ks & 0x00ff_ffff)
    } else {
        None
    }
}
