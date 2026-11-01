#![windows_subsystem = "windows"]
//! GPU tray: ikona v oznamovací oblasti s obsazením VRAM hlavní grafické karty po procesech
//! a s restartem ovladače karty (pnputil /restart-device) bez restartu Windows.

mod gpu;

use std::time::{Duration, Instant};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::*;

const REFRESH: Duration = Duration::from_secs(2);
const MAX_PROCS: usize = 15;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("GpuTray");

fn size(b: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    if b as f64 >= GB / 10.0 {
        format!("{:.1} GB", b as f64 / GB).replace('.', ",")
    } else {
        format!("{} MB", b / (1024 * 1024))
    }
}

/// Ikona 32×32: „modul paměti“, zaplněný zdola podle obsazení VRAM (zelená < 70 %, oranžová < 90 %, jinak červená).
fn usage_icon(fraction: Option<f64>) -> Icon {
    const N: usize = 32;
    let mut rgba = vec![0u8; N * N * 4];
    let (x0, x1, y0, y1) = (7usize, 24usize, 2usize, 29usize);
    let fill_color = match fraction {
        None => [128, 128, 128],
        Some(f) if f < 0.7 => [60, 180, 75],
        Some(f) if f < 0.9 => [245, 150, 30],
        Some(_) => [220, 50, 47],
    };
    let filled_from = y1 - ((y1 - y0 - 1) as f64 * fraction.unwrap_or(0.0).clamp(0.0, 1.0)).round() as usize;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let border = x == x0 || x == x1 || y == y0 || y == y1;
            let px = &mut rgba[(y * N + x) * 4..][..4];
            if border {
                px.copy_from_slice(&[200, 200, 200, 255]);
            } else if y >= filled_from {
                px.copy_from_slice(&[fill_color[0], fill_color[1], fill_color[2], 255]);
            } else {
                px.copy_from_slice(&[40, 40, 40, 160]);
            }
        }
    }
    // „piny“ po stranách
    for y in (y0 + 3..y1 - 1).step_by(4) {
        for x in [x0 - 3, x0 - 2, x1 + 2, x1 + 3] {
            rgba[(y * N + x) * 4..][..4].copy_from_slice(&[200, 200, 200, 255]);
        }
    }
    Icon::from_rgba(rgba, N as u32, N as u32).expect("ikona")
}

fn message_box(text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(HWND::default(), &HSTRING::from(text), w!("GPU tray"), flags | MB_SETFOREGROUND | MB_TOPMOST) }
}

fn exe_path() -> String {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..len])
}

fn autostart_enabled() -> bool {
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None).is_ok()
    }
}

/// Stav i v textu: u nezaškrtnuté položky Windows nic nezobrazí, takže by nebylo poznat, že jde o přepínač.
fn autostart_label(on: bool) -> &'static str {
    if on { "Spouštět po přihlášení: zapnuto" } else { "Spouštět po přihlášení: vypnuto" }
}

fn set_autostart(on: bool) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_SET_VALUE, &mut key).is_err() {
            return;
        }
        if on {
            let value: Vec<u16> = format!("\"{}\"", exe_path()).encode_utf16().chain([0]).collect();
            let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
            let _ = RegSetValueExW(key, RUN_VALUE, 0, REG_SZ, Some(bytes));
        } else {
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
        let _ = RegCloseKey(key);
    }
}

struct App {
    tray: TrayIcon,
    counters: Option<gpu::Counters>,
    last: Option<gpu::Snapshot>,
    header: MenuItem,
    shared: MenuItem,
    procs: Vec<MenuItem>,
    restart: MenuItem,
    autostart: CheckMenuItem,
    quit: MenuItem,
}

impl App {
    fn new() -> Self {
        let header = MenuItem::new("Grafická karta: čtu…", false, None);
        let shared = MenuItem::new("", false, None);
        let restart = MenuItem::new("Restartovat ovladač grafiky…", true, None);
        let on = autostart_enabled();
        let autostart = CheckMenuItem::new(autostart_label(on), true, on, None);
        let quit = MenuItem::new("Konec", true, None);
        let tray = TrayIconBuilder::new()
            .with_tooltip("GPU tray")
            .with_icon(usage_icon(None))
            .build()
            .expect("tray ikona");
        let mut app = Self {
            tray,
            counters: gpu::Counters::open(),
            last: None,
            header,
            shared,
            procs: Vec::new(),
            restart,
            autostart,
            quit,
        };
        app.rebuild_menu(0);
        app
    }

    /// Menu se staví znovu jen při změně počtu řádků; jinak se mění jen texty. Obnova neběží, dokud je menu otevřené
    /// (TrackPopupMenu má vlastní smyčku zpráv), takže se menu nemění pod rukama.
    fn rebuild_menu(&mut self, rows: usize) {
        self.procs = (0..rows).map(|_| MenuItem::new("", false, None)).collect();
        let menu = Menu::new();
        let _ = menu.append_items(&[&self.header, &self.shared, &PredefinedMenuItem::separator()]);
        for item in &self.procs {
            let _ = menu.append(item);
        }
        if rows > 0 {
            let _ = menu.append(&PredefinedMenuItem::separator());
        }
        let _ = menu.append_items(&[&self.restart, &self.autostart, &PredefinedMenuItem::separator(), &self.quit]);
        self.tray.set_menu(Some(Box::new(menu)));
    }

    fn refresh(&mut self) {
        if self.counters.is_none() {
            self.counters = gpu::Counters::open();
        }
        let snap = self.counters.as_mut().and_then(|c| c.sample());
        let Some(s) = snap else {
            self.header.set_text("Grafická karta: data nejsou k dispozici");
            let _ = self.tray.set_icon(Some(usage_icon(None)));
            let _ = self.tray.set_tooltip(Some("GPU tray: data nejsou k dispozici"));
            self.last = None;
            return;
        };

        let total = s.adapter.total_dedicated.max(1);
        let fraction = s.used_dedicated as f64 / total as f64;
        let mut lines: Vec<String> = s
            .procs
            .iter()
            .take(MAX_PROCS)
            .map(|p| {
                let mut t = format!("{}  ({})  —  {}", p.name, p.pid, size(p.dedicated));
                if p.shared >= 64 * 1024 * 1024 {
                    t += &format!("  + RAM {}", size(p.shared));
                }
                t
            })
            .collect();
        if s.procs.len() > MAX_PROCS {
            let rest: u64 = s.procs[MAX_PROCS..].iter().map(|p| p.dedicated).sum();
            lines.push(format!("… a dalších {} procesů  —  {}", s.procs.len() - MAX_PROCS, size(rest)));
        }
        if lines.len() != self.procs.len() {
            self.rebuild_menu(lines.len());
        }
        for (item, text) in self.procs.iter().zip(&lines) {
            item.set_text(text);
        }
        self.header.set_text(format!(
            "{}:  VRAM {} / {}  ({:.0} %)",
            s.adapter.name,
            size(s.used_dedicated),
            size(total),
            fraction * 100.0
        ));
        self.shared.set_text(format!("Sdílená paměť (RAM):  {}", size(s.used_shared)));

        let mut tip = format!(
            "{}\nVRAM {} / {} ({:.0} %), RAM {}",
            s.adapter.name,
            size(s.used_dedicated),
            size(total),
            fraction * 100.0,
            size(s.used_shared)
        );
        if let Some(p) = s.procs.first() {
            tip += &format!("\nnejvíc: {} {}", p.name, size(p.dedicated));
        }
        let tip: String = tip.chars().take(127).collect();
        let _ = self.tray.set_tooltip(Some(tip));
        let _ = self.tray.set_icon(Some(usage_icon(Some(fraction))));
        self.last = Some(s);
    }

    fn confirm_restart(&self) {
        let Some(s) = &self.last else {
            message_box("Grafickou kartu se nepodařilo najít.", MB_ICONERROR);
            return;
        };
        let Some(id) = gpu::device_instance_id(s.adapter.vendor, s.adapter.device) else {
            message_box(&format!("Nenašel jsem zařízení pro {}.", s.adapter.name), MB_ICONERROR);
            return;
        };
        let users: Vec<String> = s
            .procs
            .iter()
            .filter(|p| p.dedicated >= 100 * 1024 * 1024)
            .map(|p| format!("   • {} ({}) — {}", p.name, p.pid, size(p.dedicated)))
            .collect();
        let mut text = format!("Restartovat ovladač karty {}?\n\n", s.adapter.name);
        if users.is_empty() {
            text += "Kartu teď žádná aplikace výrazně nepoužívá.\n\n";
        } else {
            text += &format!(
                "Kartu teď používají:\n{}\n\nTyto aplikace mohou spadnout nebo přijít o to, co mají na GPU. \
                 Bezpečnější je nejdřív je ukončit (LM Studio, ComfyUI…).\n\n",
                users.join("\n")
            );
        }
        text += "Obrazovka na chvíli zčerná. Restart vyžaduje práva správce (okno UAC).";
        if message_box(&text, MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) != IDYES {
            return;
        }
        std::thread::spawn(move || match gpu::restart_device(&id) {
            Ok(0) => {
                message_box("Ovladač grafické karty byl restartován.", MB_ICONINFORMATION);
            }
            Ok(3010) => {
                message_box("pnputil hlásí, že restart karty vyžaduje restart Windows.", MB_ICONWARNING);
            }
            Ok(code) => {
                message_box(&format!("Restart karty selhal (pnputil vrátil kód {code})."), MB_ICONERROR);
            }
            Err(e) if e.contains("zrušil") || e.contains("cancel") => {}
            Err(e) => {
                message_box(&format!("Restart karty se nespustil: {e}"), MB_ICONERROR);
            }
        });
    }

    fn on_menu(&mut self, ev: MenuEvent) {
        if ev.id == *self.restart.id() {
            self.confirm_restart();
        } else if ev.id == *self.autostart.id() {
            // Přepínat podle registru, ne podle is_checked(): stav zaškrtnutí položky se po kliknutí
            // v tray menu nemusí změnit (menu se přestavuje), takže by se zapisovalo pořád "vypnuto".
            set_autostart(!autostart_enabled());
            let on = autostart_enabled();
            self.autostart.set_checked(on);
            self.autostart.set_text(autostart_label(on));
        } else if ev.id == *self.quit.id() {
            unsafe { PostQuitMessage(0) };
        }
    }
}

/// `gpu-tray.exe --dump <soubor>`: jednorázově zapíše načtená data (kontrola bez ikony).
fn dump(path: &str) {
    let mut out = String::new();
    match gpu::Counters::open().and_then(|mut c| c.sample()) {
        None => out += "data nejsou k dispozici\n",
        Some(s) => {
            out += &format!(
                "{} ({}), VRAM {} / {}, RAM {}\nzařízení: {}\n",
                s.adapter.name,
                s.adapter.luid,
                size(s.used_dedicated),
                size(s.adapter.total_dedicated),
                size(s.used_shared),
                gpu::device_instance_id(s.adapter.vendor, s.adapter.device).unwrap_or_else(|| "nenalezeno".into())
            );
            for p in &s.procs {
                out += &format!("{:>8}  {:<40} {:>10}  RAM {}\n", p.pid, p.name, size(p.dedicated), size(p.shared));
            }
        }
    }
    let _ = std::fs::write(path, out);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, path] = args.as_slice() {
        if flag == "--dump" {
            return dump(path);
        }
    }
    unsafe {
        let _mutex = CreateMutexW(None, true, w!("Local\\GpuTray"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let mut app = App::new();
        app.refresh();
        let mut last = Instant::now();
        // časovač jen budí smyčku, aby se vyřizovaly události menu a obnova
        SetTimer(HWND::default(), 0, 250, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                app.on_menu(ev);
            }
            if last.elapsed() >= REFRESH {
                app.refresh();
                last = Instant::now();
            }
        }
    }
}
