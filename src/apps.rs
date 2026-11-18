//! Descoberta de aplicações instaladas (arquivos .desktop do padrão XDG) e lançamento.

use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

#[derive(Clone, Debug)]
pub struct App {
    /// ID do arquivo desktop, ex.: `firefox.desktop`. Vazio para apps sintéticos (janelas sem .desktop).
    pub id: String,
    pub name: String,
    /// Texto normalizado usado pela busca do Launchpad.
    pub search: String,
    pub icon: String,
    pub exec: String,
    pub wm_class: String,
    pub path: PathBuf,
    /// Aparece no Launchpad.
    pub listed: bool,
}

impl App {
    /// App criado a partir de uma janela em execução que não corresponde a nenhum .desktop.
    pub fn synthetic(class: &str, icon_key: String) -> App {
        let mut name = class.to_string();
        if let Some(f) = name.get(0..1) {
            name = f.to_uppercase() + &name[1..];
        }
        App {
            id: String::new(),
            search: normalize(&name),
            name,
            icon: icon_key,
            exec: String::new(),
            wm_class: class.to_lowercase(),
            path: PathBuf::new(),
            listed: false,
        }
    }

    pub fn launchable(&self) -> bool {
        !self.exec.is_empty()
    }
}

pub fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var("XDG_DATA_HOME") {
        Ok(d) if !d.is_empty() => dirs.push(PathBuf::from(d)),
        _ => dirs.push(home().join(".local/share")),
    }
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let sys = if sys.is_empty() { "/usr/local/share:/usr/share".to_string() } else { sys };
    dirs.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    dirs
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

/// Lê todos os .desktop visíveis, respeitando a precedência XDG (o primeiro ID encontrado vence).
fn current_desktops() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').map(|s| s.to_lowercase()).collect()
}

/// Carrega um .desktop avulso (ex.: arrastado da área de trabalho). O ID é o caminho completo.
pub fn load_file(path: &Path) -> Option<App> {
    let mut app = parse(path, path.to_string_lossy().to_string(), &locales(), &current_desktops())?;
    app.listed = false;
    Some(app)
}

/// `file:///caminho%20com%20espaco` → caminho local (com decodificação de %XX).
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

pub fn scan() -> Vec<App> {
    let locales = locales();
    let desktops = current_desktops();
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in data_dirs() {
        let base = dir.join("applications");
        let mut files = Vec::new();
        collect(&base, &base, &mut files);
        for (id, path) in files {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(app) = parse(&path, id, &locales, &desktops) {
                apps.push(app);
            }
        }
    }
    apps.sort_by_key(|a| normalize(&a.name));
    apps
}

fn collect(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect(base, &p, out);
        } else if p.extension().is_some_and(|e| e == "desktop") {
            if let Ok(rel) = p.strip_prefix(base) {
                let id = rel.to_string_lossy().replace('/', "-");
                out.push((id, p));
            }
        }
    }
}

fn locales() -> Vec<String> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    let base = raw.split(['.', '@']).next().unwrap_or("").to_string();
    let mut v = Vec::new();
    if !base.is_empty() && base != "C" && base != "POSIX" {
        v.push(base.clone());
        if let Some((lang, _)) = base.split_once('_') {
            v.push(lang.to_string());
        }
    }
    v
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('s') => out.push(' '),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse(path: &Path, id: String, locales: &[String], desktops: &[String]) -> Option<App> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut kv: HashMap<&str, &str> = HashMap::new();
    let mut in_entry = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_entry = l == "[Desktop Entry]";
            continue;
        }
        if !in_entry || l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            kv.entry(k.trim()).or_insert(v.trim());
        }
    }
    let get = |k: &str| kv.get(k).map(|v| unescape(v));
    let truthy = |k: &str| kv.get(k).is_some_and(|v| *v == "true");
    let localized = |k: &str| {
        locales
            .iter()
            .find_map(|l| kv.get(format!("{k}[{l}]").as_str()).map(|v| unescape(v)))
            .or_else(|| get(k))
    };

    if kv.get("Type").copied() != Some("Application") || truthy("NoDisplay") || truthy("Hidden") {
        return None;
    }
    let in_list = |key: &str| {
        kv.get(key).map(|v| v.split(';').any(|d| !d.is_empty() && desktops.contains(&d.to_lowercase())))
    };
    if in_list("OnlyShowIn") == Some(false) || in_list("NotShowIn") == Some(true) {
        return None;
    }
    if let Some(te) = get("TryExec") {
        if find_in_path(&te).is_none() {
            return None;
        }
    }
    let exec = get("Exec")?;
    let name = localized("Name")?;
    let generic = localized("GenericName").unwrap_or_default();
    let keywords = localized("Keywords").unwrap_or_default();
    let stem = id.trim_end_matches(".desktop").to_string();
    let icon = get("Icon").filter(|s| !s.is_empty()).unwrap_or_else(|| "application-x-executable".into());
    Some(App {
        search: normalize(&format!("{name}\n{generic}\n{keywords}\n{stem}")),
        id,
        name,
        icon,
        wm_class: get("StartupWMClass").unwrap_or_default().to_lowercase(),
        exec,
        path: path.to_path_buf(),
        listed: true,
    })
}

pub fn find_in_path(prog: &str) -> Option<PathBuf> {
    if prog.contains('/') {
        let p = PathBuf::from(prog);
        return p.exists().then_some(p);
    }
    std::env::var("PATH")
        .ok()?
        .split(':')
        .map(|d| Path::new(d).join(prog))
        .find(|p| p.is_file())
}

/// Nome do executável principal, usado para casar janelas (WM_CLASS) com apps.
fn exec_program(exec: &str) -> Option<String> {
    let toks: Vec<&str> = exec.split_whitespace().map(|t| t.trim_matches('"')).collect();
    let mut i = 0;
    while i < toks.len() && (toks[i] == "env" || toks[i].contains('=') && !toks[i].starts_with('-')) {
        i += 1;
    }
    let prog = Path::new(toks.get(i)?).file_name()?.to_string_lossy().to_string();
    if prog == "flatpak" {
        let run = toks.iter().position(|t| *t == "run")?;
        return toks[run + 1..].iter().find(|t| !t.starts_with('-')).map(|s| s.to_string());
    }
    Some(prog)
}

/// Mapa chave-minúscula → índice do app, em ordem de confiabilidade.
pub fn build_matcher(apps: &[App]) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    let mut add = |key: String, i: usize| {
        if !key.is_empty() {
            m.entry(key).or_insert(i);
        }
    };
    for (i, a) in apps.iter().enumerate() {
        add(a.wm_class.clone(), i);
    }
    for (i, a) in apps.iter().enumerate() {
        add(a.id.trim_end_matches(".desktop").to_lowercase(), i);
    }
    for (i, a) in apps.iter().enumerate() {
        let stem = a.id.trim_end_matches(".desktop").to_lowercase();
        if let Some(last) = stem.rsplit('.').next() {
            add(last.to_string(), i);
        }
    }
    for (i, a) in apps.iter().enumerate() {
        if let Some(p) = exec_program(&a.exec) {
            let p = p.to_lowercase();
            if let Some(last) = p.rsplit('.').next() {
                add(last.to_string(), i);
            }
            add(p, i);
        }
    }
    for (i, a) in apps.iter().enumerate() {
        add(a.name.to_lowercase(), i);
    }
    m
}

/// Minúsculas e sem acentos, para busca tolerante ("musica" encontra "Música").
pub fn normalize(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            o => o,
        })
        .collect()
}

fn strip_field_codes(exec: &str) -> String {
    let mut out = String::new();
    let mut it = exec.chars();
    while let Some(c) = it.next() {
        if c == '%' {
            if let Some('%') = it.next() {
                out.push('%');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Inicia o app num grupo de processos próprio (sobrevive ao Ctrl+C da dock).
pub fn launch(app: &App) {
    static GIO: OnceLock<bool> = OnceLock::new();
    if !app.launchable() {
        return;
    }
    let mut cmd = if *GIO.get_or_init(|| find_in_path("gio").is_some()) {
        let mut c = Command::new("gio");
        c.arg("launch").arg(&app.path);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(format!("exec {}", strip_field_codes(&app.exec)));
        c
    };
    spawn(&mut cmd);
}

pub fn open_uri(uri: &str) {
    let prog = if find_in_path("gio").is_some() { ["gio", "open"] } else { ["xdg-open", ""] };
    let mut c = Command::new(prog[0]);
    if !prog[1].is_empty() {
        c.arg(prog[1]);
    }
    c.arg(uri);
    spawn(&mut c);
}

fn spawn(cmd: &mut Command) {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("rdock: falha ao iniciar processo: {e}"),
    }
}
