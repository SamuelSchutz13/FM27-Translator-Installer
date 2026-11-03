// Sem janela de console no .exe final.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

const REPO: &str = "https://github.com/arthurabreu/fm27-ptbr-release";
const API: &str = "https://api.github.com/repos/arthurabreu/fm27-ptbr-release/releases";
const FILE: &str = "portuguese_br.ltc";
const STEAM_APP_ID: &str = "4530370";

/// Pasta `languages` do FM27. Steam e Epic usam a mesma pasta de Documentos no Windows.
fn languages_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return Some(dirs::data_dir()?.join("Football Manager 27").join("languages"));
    }
    let docs = if cfg!(target_os = "linux") {
        // Linux só roda via Steam/Proton: os "Documentos" ficam dentro do prefixo Wine.
        dirs::data_dir()?
            .join("Steam/steamapps/compatdata")
            .join(STEAM_APP_ID)
            .join("pfx/drive_c/users/steamuser/Documents")
    } else {
        // document_dir respeita Documentos redirecionado (ex.: OneDrive).
        dirs::document_dir()?
    };
    Some(docs.join("Sports Interactive").join("Football Manager 27").join("languages"))
}

/// O usuário pode escolher a pasta do FM27 ou a própria `languages`; as duas valem.
fn to_languages(picked: PathBuf) -> PathBuf {
    if picked.file_name().is_some_and(|n| n.eq_ignore_ascii_case("languages")) {
        picked
    } else {
        picked.join("languages")
    }
}

// Configurações do instalador (versão instalada, pasta escolhida) ficam fora da pasta do jogo.
fn cfg_path(name: &str) -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("fm27-ptbr-installer").join(name))
}
fn read_cfg(name: &str) -> Option<String> {
    fs::read_to_string(cfg_path(name)?).ok().map(|s| s.trim().to_string())
}
fn write_cfg(name: &str, value: &str) {
    if let Some(p) = cfg_path(name) {
        let _ = fs::create_dir_all(p.parent().unwrap());
        let _ = fs::write(p, value);
    }
}
fn remove_cfg(name: &str) {
    if let Some(p) = cfg_path(name) {
        let _ = fs::remove_file(p);
    }
}

/// Versão instalada: só existe se o .ltc estiver na pasta. Sem registro = instalado à mão.
fn installed_version(dir: &Path) -> Option<String> {
    dir.join(FILE)
        .exists()
        .then(|| read_cfg("versao").unwrap_or_else(|| "desconhecida".into()))
}

fn game_running() -> bool {
    // shortcut: procura "fm.exe"/"fm", o nome usado nos FMs anteriores; ajustar se o FM27 mudar o executável.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000; // evita piscar um console a cada checagem
        std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq fm.exe", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("fm.exe"))
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("pgrep")
            .args(["-x", "fm"])
            .output()
            .is_ok_and(|o| o.status.success())
    }
}

fn open_folder(dir: &Path) {
    let cmd = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(dir).spawn();
}

struct Release {
    name: String,
    tag: String,
    date: String,
    size: u64,
    url: String,
    notes: String,
    prerelease: bool,
}

/// Releases que têm o arquivo de tradução, da mais nova para a mais antiga (ordem do GitHub).
fn fetch_releases() -> Result<Vec<Release>, String> {
    let list: Vec<serde_json::Value> = ureq::get(API)
        .call()
        .map_err(|e| format!("falha ao consultar o GitHub: {e}"))?
        .into_json()
        .map_err(|e| format!("resposta inválida do GitHub: {e}"))?;
    let s = |v: &serde_json::Value| v.as_str().unwrap_or("").to_string();
    let releases: Vec<Release> = list
        .iter()
        .filter_map(|r| {
            let asset = r["assets"].as_array()?.iter().find(|x| x["name"] == FILE)?;
            Some(Release {
                name: s(&r["name"]),
                tag: s(&r["tag_name"]),
                // published_at vem como "2026-11-03T01:40:23Z"; os 10 primeiros chars são a data.
                date: s(&r["published_at"]).chars().take(10).collect(),
                size: asset["size"].as_u64().unwrap_or(0),
                url: asset["browser_download_url"].as_str()?.into(),
                notes: s(&r["body"]),
                prerelease: r["prerelease"].as_bool().unwrap_or(false),
            })
        })
        .collect();
    if releases.is_empty() {
        return Err(format!("nenhuma release com o arquivo {FILE}"));
    }
    Ok(releases)
}

fn install(url: &str, dir: &Path, progress: impl Fn(f32)) -> Result<PathBuf, String> {
    let resp = ureq::get(url).call().map_err(|e| format!("falha no download: {e}"))?;
    let total: f32 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let mut reader = resp.into_reader();
    let mut bytes = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("falha ao baixar: {e}"))?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        if total > 0.0 {
            progress(bytes.len() as f32 / total);
        }
    }
    if bytes.is_empty() {
        return Err("o arquivo baixado veio vazio".into());
    }
    fs::create_dir_all(dir).map_err(|e| format!("não consegui criar {}: {e}", dir.display()))?;
    let dest = dir.join(FILE);
    // Grava num .tmp e renomeia: um download interrompido não corrompe a tradução já instalada.
    let tmp = dir.join(format!("{FILE}.tmp"));
    fs::write(&tmp, &bytes).map_err(|e| format!("falha ao gravar: {e}"))?;
    fs::rename(&tmp, &dest).map_err(|e| format!("falha ao substituir o arquivo (o jogo está aberto?): {e}"))?;
    Ok(dest)
}

enum Status {
    Loading,
    Idle,
    Downloading(f32),
}

struct State {
    status: Status,
    releases: Vec<Release>,
    selected: usize,
    /// Índice da release estável mais recente (marcada como "mais recente" no seletor).
    latest: usize,
    /// (sucesso?, texto) da última ação.
    message: Option<(bool, String)>,
}

struct App {
    dir: Option<PathBuf>,
    state: Arc<Mutex<State>>,
    game_running: Arc<AtomicBool>,
    confirm_uninstall: bool,
}

enum Action {
    Install,
    Retry,
    OpenFolder,
    PickFolder,
    AskUninstall,
    CancelUninstall,
    Uninstall,
}

impl App {
    fn load(&self, ctx: &egui::Context) {
        {
            let mut s = self.state.lock().unwrap();
            s.status = Status::Loading;
            s.message = None;
        }
        let (state, ctx) = (self.state.clone(), ctx.clone());
        thread::spawn(move || {
            let result = fetch_releases();
            let mut s = state.lock().unwrap();
            match result {
                Ok(list) => {
                    s.latest = list.iter().position(|r| !r.prerelease).unwrap_or(0);
                    s.selected = s.latest;
                    s.releases = list;
                }
                Err(e) => s.message = Some((false, e)),
            }
            s.status = Status::Idle;
            ctx.request_repaint();
        });
    }

    fn start_install(&self, ctx: &egui::Context) {
        let mut s = self.state.lock().unwrap();
        let (Some(r), Some(dir)) = (s.releases.get(s.selected), self.dir.clone()) else { return };
        let (url, tag) = (r.url.clone(), r.tag.clone());
        s.status = Status::Downloading(0.0);
        s.message = None;
        let (state, ctx) = (self.state.clone(), ctx.clone());
        thread::spawn(move || {
            let result = install(&url, &dir, |p| {
                state.lock().unwrap().status = Status::Downloading(p);
                ctx.request_repaint();
            });
            let mut s = state.lock().unwrap();
            s.status = Status::Idle;
            s.message = Some(match result {
                Ok(_) => {
                    write_cfg("versao", &tag);
                    (true, format!(
                        "✔ {tag} instalada! Reinicie o FM27 e vá em Preferências → Idioma → \"Português (Brasil)\"."
                    ))
                }
                Err(e) => (false, format!("Erro: {e}")),
            });
            ctx.request_repaint();
        });
    }

    fn uninstall(&self) {
        let Some(dir) = &self.dir else { return };
        let msg = match fs::remove_file(dir.join(FILE)) {
            Ok(()) => {
                remove_cfg("versao");
                (true, "Tradução removida. Reinicie o FM27 e escolha outro idioma.".into())
            }
            Err(e) => (false, format!("Erro ao remover (o jogo está aberto?): {e}")),
        };
        self.state.lock().unwrap().message = Some(msg);
    }

    fn pick_folder(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("Escolha a pasta do Football Manager 27");
        if let Some(start) = self.dir.as_ref().and_then(|d| d.parent()).filter(|p| p.exists()) {
            dialog = dialog.set_directory(start);
        }
        if let Some(picked) = dialog.pick_folder() {
            let dir = to_languages(picked);
            write_cfg("pasta", &dir.to_string_lossy());
            self.dir = Some(dir);
            self.state.lock().unwrap().message = None;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let mut action = None;
        let running = self.game_running.load(Ordering::Relaxed);
        let installed = self.dir.as_deref().and_then(installed_version);
        let game_found = self.dir.as_ref().and_then(|d| d.parent()).is_some_and(|p| p.exists());

        egui::TopBottomPanel::bottom("links").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.hyperlink_to("Reportar erro de tradução", format!("{REPO}/issues"));
                ui.label("·");
                ui.hyperlink_to("Ver no GitHub", REPO);
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let mut s = self.state.lock().unwrap();
            let busy = matches!(s.status, Status::Downloading(_));
            let warn = ui.visuals().warn_fg_color;
            ui.heading("Tradução PT-BR — Football Manager 27");
            ui.separator();

            if !s.releases.is_empty() {
                let label = |s: &State, i: usize| {
                    let r = &s.releases[i];
                    let extra = if i == s.latest {
                        "  (mais recente)"
                    } else if r.prerelease {
                        "  (pré-release)"
                    } else {
                        ""
                    };
                    format!("{} — {}{extra}", r.tag, r.date)
                };
                ui.horizontal(|ui| {
                    ui.strong("Escolha a versão:");
                    ui.add_enabled_ui(!busy, |ui| {
                        let current = label(&s, s.selected);
                        egui::ComboBox::from_id_salt("release")
                            .width(260.0)
                            .selected_text(current)
                            .show_ui(ui, |ui| {
                                for i in 0..s.releases.len() {
                                    let text = label(&s, i);
                                    ui.selectable_value(&mut s.selected, i, text);
                                }
                            });
                    });
                });
            }

            ui.horizontal(|ui| {
                ui.strong("Instalada:");
                match &installed {
                    Some(v) => {
                        ui.label(v);
                        if let Some(l) = s.releases.get(s.latest) {
                            if &l.tag != v {
                                ui.colored_label(warn, format!("→ atualização disponível: {}", l.tag));
                            }
                        }
                    }
                    None => {
                        ui.weak("nenhuma");
                    }
                }
            });
            ui.add_space(6.0);

            if let Some(r) = s.releases.get(s.selected) {
                egui::Grid::new("info").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.strong("Release:");
                    ui.label(&r.name);
                    ui.end_row();
                    ui.strong("Publicada:");
                    ui.label(&r.date);
                    ui.end_row();
                    ui.strong("Tamanho:");
                    ui.label(format!("{:.1} MB", r.size as f64 / 1_048_576.0));
                    ui.end_row();
                });
                if !r.notes.trim().is_empty() {
                    egui::CollapsingHeader::new("Notas da versão").default_open(true).show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(110.0).show(ui, |ui| {
                            // Markdown básico: títulos em negrito, sem ** e crases.
                            for line in r.notes.lines() {
                                let clean = line.replace("**", "").replace('`', "");
                                let title = clean.trim_start_matches('#');
                                if title.len() != clean.len() {
                                    ui.strong(title.trim());
                                } else {
                                    ui.label(clean);
                                }
                            }
                        });
                    });
                }
            }
            ui.separator();

            ui.horizontal(|ui| {
                ui.strong("Pasta:");
                let path = self.dir.as_ref().map_or("não encontrada".into(), |d| d.display().to_string());
                ui.label(egui::RichText::new(path).small().weak());
            });
            ui.horizontal(|ui| {
                let can_open = self.dir.as_ref().is_some_and(|d| d.exists() || d.parent().is_some_and(|p| p.exists()));
                if ui.add_enabled(can_open, egui::Button::new("Abrir pasta")).clicked() {
                    action = Some(Action::OpenFolder);
                }
                if ui.add_enabled(!busy, egui::Button::new("Alterar pasta...")).clicked() {
                    action = Some(Action::PickFolder);
                }
            });
            if !game_found {
                ui.colored_label(warn, "Pasta do FM27 não encontrada. Abra o jogo uma vez ou use \"Alterar pasta...\".");
            }
            if running {
                ui.colored_label(warn, "⚠ O FM27 está aberto. Feche o jogo antes de instalar ou remover a tradução.");
            }
            ui.add_space(10.0);

            match &s.status {
                Status::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Procurando as releases...");
                    });
                }
                Status::Downloading(p) => {
                    ui.label("Baixando e instalando...");
                    ui.add(egui::ProgressBar::new(*p).show_percentage());
                }
                Status::Idle if s.releases.is_empty() => {
                    if ui.button("Tentar novamente").clicked() {
                        action = Some(Action::Retry);
                    }
                }
                Status::Idle => {
                    ui.horizontal(|ui| {
                        let btn = egui::Button::new(egui::RichText::new("Instalar tradução").size(16.0))
                            .min_size(egui::vec2(180.0, 34.0));
                        if ui.add_enabled(game_found, btn).clicked() {
                            action = Some(Action::Install);
                        }
                        if installed.is_some() {
                            if self.confirm_uninstall {
                                ui.label("Remover a tradução?");
                                if ui.button("Sim, remover").clicked() {
                                    action = Some(Action::Uninstall);
                                }
                                if ui.button("Cancelar").clicked() {
                                    action = Some(Action::CancelUninstall);
                                }
                            } else if ui.button("Desinstalar").clicked() {
                                action = Some(Action::AskUninstall);
                            }
                        }
                    });
                }
            }
            if let Some((ok, msg)) = &s.message {
                ui.add_space(6.0);
                let color = if *ok { egui::Color32::from_rgb(60, 170, 90) } else { ui.visuals().error_fg_color };
                ui.colored_label(color, msg);
            }
        });

        match action {
            Some(Action::Install) => self.start_install(ctx),
            Some(Action::Retry) => self.load(ctx),
            Some(Action::OpenFolder) => {
                if let Some(d) = &self.dir {
                    open_folder(if d.exists() { d } else { d.parent().unwrap() });
                }
            }
            Some(Action::PickFolder) => self.pick_folder(),
            Some(Action::AskUninstall) => self.confirm_uninstall = true,
            Some(Action::CancelUninstall) => self.confirm_uninstall = false,
            Some(Action::Uninstall) => {
                self.confirm_uninstall = false;
                self.uninstall();
            }
            None => {}
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([600.0, 560.0])
            .with_min_inner_size([520.0, 420.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap()),
        ..Default::default()
    };
    eframe::run_native(
        "Instalador Tradução PT-BR FM27",
        options,
        Box::new(|cc| {
            // A fonte padrão do egui não tem setas (→) nem alguns símbolos; usa as do Windows como reserva.
            let mut fonts = egui::FontDefinitions::default();
            for (name, file) in [("segoe", "segoeui.ttf"), ("segoe-sym", "seguisym.ttf")] {
                if let Ok(bytes) = fs::read(format!("C:\\Windows\\Fonts\\{file}")) {
                    fonts.font_data.insert(name.into(), egui::FontData::from_owned(bytes));
                    fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().push(name.into());
                }
            }
            cc.egui_ctx.set_fonts(fonts);

            let app = App {
                dir: read_cfg("pasta").map(PathBuf::from).or_else(languages_dir),
                state: Arc::new(Mutex::new(State {
                    status: Status::Loading,
                    releases: Vec::new(),
                    selected: 0,
                    latest: 0,
                    message: None,
                })),
                game_running: Arc::new(AtomicBool::new(false)),
                confirm_uninstall: false,
            };

            let (flag, ctx) = (app.game_running.clone(), cc.egui_ctx.clone());
            thread::spawn(move || loop {
                let now = game_running();
                if flag.swap(now, Ordering::Relaxed) != now {
                    ctx.request_repaint();
                }
                thread::sleep(Duration::from_secs(2));
            });

            if app.dir.is_none() {
                app.state.lock().unwrap().message =
                    Some((false, "Não encontrei a pasta Documentos. Use \"Alterar pasta...\".".into()));
            }
            app.load(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn languages_dir_aponta_para_fm27() {
        let dir = super::languages_dir().unwrap();
        assert!(dir.ends_with("Football Manager 27/languages"));
    }

    #[test]
    fn pasta_escolhida_vira_languages() {
        let fm = PathBuf::from("D:/Jogos/Football Manager 27");
        assert_eq!(super::to_languages(fm.clone()), fm.join("languages"));
        assert_eq!(super::to_languages(fm.join("Languages")), fm.join("Languages"));
    }

    #[test]
    fn install_grava_arquivo_e_reporta_progresso() {
        // Usa um servidor HTTP local de um único pedido para não depender da internet.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut c, _) = listener.accept().unwrap();
            c.read(&mut [0u8; 1024]).unwrap();
            c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        });
        let dir = std::env::temp_dir().join(format!("fm27-test-{}", std::process::id()));
        let last = std::cell::Cell::new(0.0);
        let dest = super::install(&url, &dir, |p| last.set(p)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        assert_eq!(last.get(), 1.0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
