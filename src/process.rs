//! Finding the running FL27 process and reading its memory (Linux and Windows).

use anyhow::{Result, bail};

/// Executable name of Football Life 2027.
pub const EXE_NAME: &str = "FL_2027.exe";

/// A readable, writable memory region of the game process.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub start: u64,
    pub len: u64,
}

/// Anything that can read bytes from the game's address space.
/// Tests use an in-memory fake; the app uses [`GameProcess`].
pub trait Memory {
    fn read(&self, addr: u64, len: usize) -> Option<Vec<u8>>;
    fn regions(&self) -> Vec<Region>;
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::fs::{self, File};
    use std::os::unix::fs::FileExt;

    pub struct GameProcess {
        pub pid: u32,
        mem: File,
    }

    impl GameProcess {
        pub fn find() -> Result<Option<Self>> {
            let mut best: Option<(u32, u64)> = None;
            for e in fs::read_dir("/proc")?.flatten() {
                let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                let Ok(cmd) = fs::read(format!("/proc/{pid}/cmdline")) else {
                    continue;
                };
                let cmd = cmd.strip_suffix(b"\0").unwrap_or(&cmd);
                let first = cmd.split(|b| *b == 0).next().unwrap_or(&[]);
                let hit = |s: &[u8]| {
                    s.ends_with(EXE_NAME.as_bytes())
                        || s.ends_with(EXE_NAME.to_lowercase().as_bytes())
                };
                if !(hit(cmd) || hit(first)) {
                    continue;
                }
                let rss = fs::read_to_string(format!("/proc/{pid}/statm"))
                    .ok()
                    .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .unwrap_or(0);
                if best.is_none_or(|(_, r)| rss > r) {
                    best = Some((pid, rss));
                }
            }
            let Some((pid, _)) = best else {
                return Ok(None);
            };
            match File::open(format!("/proc/{pid}/mem")) {
                Ok(mem) => Ok(Some(Self { pid, mem })),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!(
                    "FL27 is running (pid {pid}), but Linux does not allow reading its memory.\n\
                     Fix it once with:\n  sudo setcap cap_sys_ptrace+ep \"{}\"\n\
                     or for this boot only:\n  sudo sysctl kernel.yama.ptrace_scope=0",
                    std::env::current_exe()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "pesm-coaching-zone".into())
                ),
                Err(e) => Err(e.into()),
            }
        }
    }

    impl Memory for GameProcess {
        fn read(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
            let mut buf = vec![0u8; len];
            match self.mem.read_exact_at(&mut buf, addr) {
                Ok(()) => Some(buf),
                Err(_) => None,
            }
        }

        fn regions(&self) -> Vec<Region> {
            let Ok(maps) = fs::read_to_string(format!("/proc/{}/maps", self.pid)) else {
                return vec![];
            };
            maps.lines()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    let range = it.next()?;
                    let perms = it.next()?;
                    if !perms.starts_with("rw") {
                        return None;
                    }
                    let (lo, hi) = range.split_once('-')?;
                    let lo = u64::from_str_radix(lo, 16).ok()?;
                    let hi = u64::from_str_radix(hi, 16).ok()?;
                    Some(Region {
                        start: lo,
                        len: hi - lo,
                    })
                })
                .collect()
        }
    }

    pub fn is_alive(p: &GameProcess) -> bool {
        std::path::Path::new(&format!("/proc/{}", p.pid)).exists()
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQueryEx,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    pub struct GameProcess {
        pub pid: u32,
        h: HANDLE,
    }
    unsafe impl Send for GameProcess {}
    unsafe impl Sync for GameProcess {}

    impl Drop for GameProcess {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.h) };
        }
    }

    impl GameProcess {
        pub fn find() -> Result<Option<Self>> {
            let pid = unsafe {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snap == INVALID_HANDLE_VALUE {
                    bail!("could not list processes");
                }
                let mut e: PROCESSENTRY32W = std::mem::zeroed();
                e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
                let mut found = None;
                if Process32FirstW(snap, &mut e) != 0 {
                    loop {
                        let n = e
                            .szExeFile
                            .iter()
                            .position(|c| *c == 0)
                            .unwrap_or(e.szExeFile.len());
                        let name = String::from_utf16_lossy(&e.szExeFile[..n]);
                        if name.eq_ignore_ascii_case(EXE_NAME) {
                            found = Some(e.th32ProcessID);
                            break;
                        }
                        if Process32NextW(snap, &mut e) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snap);
                found
            };
            let Some(pid) = pid else { return Ok(None) };
            let h = unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION, 0, pid) };
            if h.is_null() {
                bail!(
                    "FL27 is running (pid {pid}), but Windows refused access. Try starting PESM Coaching Zone as the same user as the game, or as administrator."
                );
            }
            Ok(Some(Self { pid, h }))
        }
    }

    impl Memory for GameProcess {
        fn read(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
            let mut buf = vec![0u8; len];
            let mut got = 0usize;
            let ok = unsafe {
                ReadProcessMemory(self.h, addr as _, buf.as_mut_ptr() as _, len, &mut got)
            };
            (ok != 0 && got == len).then_some(buf)
        }

        fn regions(&self) -> Vec<Region> {
            let mut out = vec![];
            let mut addr: usize = 0;
            unsafe {
                let mut mbi: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
                while VirtualQueryEx(
                    self.h,
                    addr as _,
                    &mut mbi,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                ) != 0
                {
                    if mbi.State == MEM_COMMIT && mbi.Protect == PAGE_READWRITE {
                        out.push(Region {
                            start: mbi.BaseAddress as u64,
                            len: mbi.RegionSize as u64,
                        });
                    }
                    let next = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize);
                    if next <= addr {
                        break;
                    }
                    addr = next;
                }
            }
            out
        }
    }

    pub fn is_alive(p: &GameProcess) -> bool {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(p.h, &mut code) != 0 && code == STILL_ACTIVE as u32 }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;
    pub struct GameProcess {
        pub pid: u32,
    }
    impl GameProcess {
        pub fn find() -> Result<Option<Self>> {
            bail!("only Windows and Linux are supported")
        }
    }
    impl Memory for GameProcess {
        fn read(&self, _: u64, _: usize) -> Option<Vec<u8>> {
            None
        }
        fn regions(&self) -> Vec<Region> {
            vec![]
        }
    }
    pub fn is_alive(_: &GameProcess) -> bool {
        false
    }
}

pub use imp::{GameProcess, is_alive};
