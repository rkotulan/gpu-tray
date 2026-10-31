//! Čtení obsazené VRAM (výkonnostní čítače GPU ve Windows, nezávislé na výrobci) a restart ovladače karty.

use std::collections::{BTreeMap, HashMap};
use std::mem::size_of;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Performance::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

// v crate windows 0.58 jsou handly PDH obyčejné isize
type PdhQuery = isize;
type PdhCounter = isize;

#[derive(Clone)]
pub struct Adapter {
    pub name: String,
    /// Část názvu instance čítače, např. `luid_0x00000000_0x00b13e23`.
    pub luid: String,
    pub total_dedicated: u64,
    pub vendor: u32,
    pub device: u32,
}

pub struct ProcUsage {
    pub pid: u32,
    pub name: String,
    pub dedicated: u64,
    pub shared: u64,
}

pub struct Snapshot {
    pub adapter: Adapter,
    pub used_dedicated: u64,
    pub used_shared: u64,
    /// Seřazeno podle vyhrazené VRAM sestupně.
    pub procs: Vec<ProcUsage>,
}

/// Karta s největší vyhrazenou pamětí (diskrétní GPU). LUID se po restartu karty mění, proto se hledá při každém čtení.
pub fn main_adapter() -> Option<Adapter> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut best: Option<Adapter> = None;
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
        let candidate = Adapter {
            name: String::from_utf16_lossy(&desc.Description[..len]),
            luid: format!("luid_0x{:08x}_0x{:08x}", desc.AdapterLuid.HighPart as u32, desc.AdapterLuid.LowPart),
            total_dedicated: desc.DedicatedVideoMemory as u64,
            vendor: desc.VendorId,
            device: desc.DeviceId,
        };
        if best.as_ref().map_or(true, |b| candidate.total_dedicated > b.total_dedicated) {
            best = Some(candidate);
        }
    }
    best
}

pub struct Counters {
    query: PdhQuery,
    proc_dedicated: PdhCounter,
    proc_shared: PdhCounter,
    adapter_dedicated: PdhCounter,
    adapter_shared: PdhCounter,
}

impl Counters {
    pub fn open() -> Option<Self> {
        unsafe {
            let mut query = PdhQuery::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let add = |path: PCWSTR| {
                let mut c = PdhCounter::default();
                PdhAddEnglishCounterW(query, path, 0, &mut c);
                c
            };
            Some(Self {
                query,
                proc_dedicated: add(w!("\\GPU Process Memory(*)\\Dedicated Usage")),
                proc_shared: add(w!("\\GPU Process Memory(*)\\Shared Usage")),
                adapter_dedicated: add(w!("\\GPU Adapter Memory(*)\\Dedicated Usage")),
                adapter_shared: add(w!("\\GPU Adapter Memory(*)\\Shared Usage")),
            })
        }
    }

    pub fn sample(&mut self) -> Option<Snapshot> {
        let adapter = main_adapter()?;
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return None;
        }
        let sum_for = |counter, luid: &str| -> u64 {
            read_array(counter).iter().filter(|(n, _)| n.contains(luid)).map(|(_, v)| *v).sum()
        };
        let used_dedicated = sum_for(self.adapter_dedicated, &adapter.luid);
        let used_shared = sum_for(self.adapter_shared, &adapter.luid);

        // instance "pid_37796_luid_0x00000000_0x00b13e23_phys_0"
        let mut per_pid: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
        for (slot, counter) in [(0, self.proc_dedicated), (1, self.proc_shared)] {
            for (name, value) in read_array(counter) {
                if !name.contains(&adapter.luid) {
                    continue;
                }
                let Some(pid) = name.strip_prefix("pid_").and_then(|s| s.split('_').next()).and_then(|s| s.parse().ok())
                else {
                    continue;
                };
                let e = per_pid.entry(pid).or_default();
                if slot == 0 { e.0 += value } else { e.1 += value }
            }
        }
        let names = process_names();
        let mut procs: Vec<ProcUsage> = per_pid
            .into_iter()
            .filter(|(_, (d, s))| *d + *s > 0)
            .map(|(pid, (dedicated, shared))| ProcUsage {
                pid,
                name: names.get(&pid).cloned().unwrap_or_else(|| format!("pid {pid}")),
                dedicated,
                shared,
            })
            .collect();
        procs.sort_by(|a, b| b.dedicated.cmp(&a.dedicated).then(b.shared.cmp(&a.shared)));
        Some(Snapshot { adapter, used_dedicated, used_shared, procs })
    }
}

impl Drop for Counters {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

fn read_array(counter: PdhCounter) -> Vec<(String, u64)> {
    unsafe {
        let (mut size, mut count) = (0u32, 0u32);
        let r = PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, None);
        if r != PDH_MORE_DATA || size == 0 {
            return Vec::new();
        }
        // u64 kvůli zarovnání položek; jména instancí leží v témže bufferu za položkami
        let mut buf = vec![0u64; size as usize / 8 + 1];
        let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        if PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, Some(items)) != 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(items, count as usize)
            .iter()
            // PDH_CSTATUS_VALID_DATA (0) i PDH_CSTATUS_NEW_DATA (1) jsou platná data
            .filter(|it| it.FmtValue.CStatus <= 1)
            // PDH vrací LUID velkými písmeny, Get-Counter malými – porovnává se v malých
            .map(|it| (it.szName.to_string().unwrap_or_default().to_lowercase(),it.FmtValue.Anonymous.largeValue.max(0) as u64))
            .collect()
    }
}

/// Jména všech procesů ze snímku ToolHelp – na rozdíl od OpenProcess vidí i systémové (dwm.exe, csrss.exe).
fn process_names() -> HashMap<u32, String> {
    let mut names = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return names };
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            names.insert(entry.th32ProcessID, String::from_utf16_lossy(&entry.szExeFile[..len]));
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    names
}

/// ID instance zařízení (PCI\VEN_1002&DEV_7550&...) pro pnputil podle VendorId/DeviceId z DXGI.
pub fn device_instance_id(vendor: u32, device: u32) -> Option<String> {
    let needle = format!("VEN_{vendor:04X}&DEV_{device:04X}");
    unsafe {
        let set = SetupDiGetClassDevsW(Some(&GUID_DEVCLASS_DISPLAY), PCWSTR::null(), HWND::default(), DIGCF_PRESENT).ok()?;
        let mut found = None;
        for i in 0.. {
            let mut info = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInfo(set, i, &mut info).is_err() {
                break;
            }
            let mut buf = [0u16; 512];
            if SetupDiGetDeviceInstanceIdW(set, &info, Some(&mut buf), None).is_err() {
                continue;
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let id = String::from_utf16_lossy(&buf[..len]);
            if id.to_uppercase().contains(&needle) {
                found = Some(id);
                break;
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
        found
    }
}

/// `pnputil /restart-device` s právy správce (okno UAC). Vrací návratový kód pnputil (0 = OK, 3010 = nutný restart PC).
/// Záměrně ne vypnutí a zapnutí zařízení: Disable-PnpDevice na běžící kartě 31. 10. 2026 shodilo celé Windows.
pub fn restart_device(instance_id: &str) -> Result<u32, String> {
    let params = HSTRING::from(format!("/restart-device \"{instance_id}\""));
    unsafe {
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: w!("runas"),
            lpFile: w!("pnputil.exe"),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        ShellExecuteExW(&mut sei).map_err(|e| e.message().to_string())?;
        if sei.hProcess.is_invalid() {
            return Err("pnputil se nespustil".into());
        }
        WaitForSingleObject(sei.hProcess, INFINITE);
        let mut code = 0u32;
        let r = GetExitCodeProcess(sei.hProcess, &mut code);
        let _ = CloseHandle(sei.hProcess);
        r.map_err(|e| e.message().to_string())?;
        Ok(code)
    }
}
