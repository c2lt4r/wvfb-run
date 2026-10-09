//! wvfb-run: run a command on a hidden Win32 desktop, like xvfb-run.
//!
//! Exit codes: the command's own, 124 on timeout or hang (window not responding
//! while the tree uses ~no CPU), 125 if a dialog box appeared on the hidden
//! desktop (nobody can click it), 127 on launch failure.

use std::{
    ffi::{OsStr, OsString},
    iter::repeat_n,
    os::windows::ffi::OsStrExt,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        System::{
            Console::*, Diagnostics::Debug::*, JobObjects::*, StationsAndDesktops::*,
            Threading::*,
        },
        UI::WindowsAndMessaging::*,
    },
    core::*,
};

const USAGE: &str = "usage: wvfb-run [--timeout SECS (default 30)] [--] COMMAND [ARGS...]";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
// Fixed hang heuristic. A GPU-bound frame >60s with a blocked main thread
// would be killed; make these flags if that ever happens.
const HANG_LIMIT: Duration = Duration::from_secs(60);
const IDLE_CORES: f64 = 0.1; // below this average CPU use, a hung tree is stuck, not busy

fn main() {
    let code = run().unwrap_or_else(|e| {
        eprintln!("wvfb-run: {e}");
        127
    });
    std::process::exit(code);
}

#[derive(Debug, PartialEq)]
enum Cli {
    Help,
    Run { timeout: Duration, argv: Vec<OsString> },
}

/// Our flags come first; everything from COMMAND on is passed through untouched.
fn parse_args(args: impl IntoIterator<Item = OsString>) -> std::result::Result<Cli, &'static str> {
    let mut args = args.into_iter().peekable();
    let mut timeout = DEFAULT_TIMEOUT;
    while let Some(a) = args.peek().and_then(|a| a.to_str()) {
        match a {
            "--timeout" => {
                args.next();
                let secs: f64 = args.next().and_then(|s| s.to_str()?.parse().ok()).ok_or(USAGE)?;
                timeout = Duration::try_from_secs_f64(secs).map_err(|_| USAGE)?;
            }
            "--" => {
                args.next();
                break;
            }
            "-h" | "--help" => return Ok(Cli::Help),
            _ => break,
        }
    }
    let argv: Vec<OsString> = args.collect();
    if argv.is_empty() {
        return Err(USAGE);
    }
    Ok(Cli::Run { timeout, argv })
}

fn run() -> std::result::Result<i32, Box<dyn std::error::Error>> {
    let (timeout, argv) = match parse_args(std::env::args_os().skip(1))? {
        Cli::Help => {
            println!("{USAGE}");
            return Ok(0);
        }
        Cli::Run { timeout, argv } => (timeout, argv),
    };
    let mut cmdline = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            cmdline.push(' ' as u16);
        }
        quote(a, &mut cmdline);
    }
    cmdline.push(0);

    unsafe {
        // Desktop lives on our own window station; destroyed when the last handle/thread goes.
        let name = format!("wvfb-{}", std::process::id());
        let hdesk = CreateDesktopW(&HSTRING::from(&name), None, None, DESKTOP_CONTROL_FLAGS(0), GENERIC_ALL.0, None)?;
        let mut desk: Vec<u16> = name.encode_utf16().chain([0]).collect();

        // Job: kill the whole tree when we exit, and crash instead of showing the WER dialog.
        let job = CreateJobObjectW(None, None)?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as _,
            size_of_val(&info) as u32,
        )?;
        // Inherited by children: no "missing DLL" / "insert disk" / crash boxes.
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);

        // Pass our stdio through, so piped output (e.g. from WSL) reaches the caller.
        let std = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|s| {
            let h = GetStdHandle(s).unwrap_or_default();
            let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            h
        });
        let si = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desk.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: std[0],
            hStdOutput: std[1],
            hStdError: std[2],
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessW(
            None,
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_SUSPENDED,
            None,
            None,
            &si,
            &mut pi,
        )
        .map_err(|e| format!("cannot run {}: {e}", argv[0].display()))?;
        AssignProcessToJobObject(job, pi.hProcess)?;
        ResumeThread(pi.hThread);

        let deadline = Instant::now() + timeout;
        let mut hang = HangWatch::default();
        loop {
            if WaitForSingleObject(pi.hProcess, 200) == WAIT_OBJECT_0 {
                let mut code = 0;
                GetExitCodeProcess(pi.hProcess, &mut code)?;
                let _ = CloseDesktop(hdesk);
                return Ok(code as i32);
            }
            let scan = scan_desktop(hdesk);
            if let Some(text) = scan.dialog {
                eprintln!("wvfb-run: dialog on hidden desktop, killing: {text}");
                let _ = TerminateJobObject(job, 125);
                return Ok(125);
            }
            let now = Instant::now();
            if hang.stuck(scan.hung, job_cpu(job), now) {
                eprintln!("wvfb-run: not responding and idle for {}s, killing", HANG_LIMIT.as_secs());
                let _ = TerminateJobObject(job, 124);
                return Ok(124);
            }
            if now >= deadline {
                eprintln!("wvfb-run: timed out after {}s, killing", timeout.as_secs());
                let _ = TerminateJobObject(job, 124);
                return Ok(124);
            }
        }
    }
}

/// Hung window + busy CPU = working (long main-thread job), keep going.
/// Hung window + ~idle CPU for HANG_LIMIT = stuck (deadlock), kill.
#[derive(Default)]
struct HangWatch {
    since: Option<(Instant, i64)>,
}

impl HangWatch {
    fn stuck(&mut self, hung: bool, cpu: i64, now: Instant) -> bool {
        if !hung {
            self.since = None;
            return false;
        }
        let (t0, c0) = *self.since.get_or_insert((now, cpu));
        let elapsed = now - t0;
        if elapsed < HANG_LIMIT {
            return false;
        }
        let cores = (cpu - c0) as f64 / 1e7 / elapsed.as_secs_f64(); // cpu is in 100ns units
        if cores < IDLE_CORES {
            return true;
        }
        self.since = Some((now, cpu)); // busy: judge the next window afresh
        false
    }
}

/// Total user+kernel CPU of every process ever in the job, in 100ns units.
fn job_cpu(job: HANDLE) -> i64 {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    unsafe {
        let _ = QueryInformationJobObject(
            Some(job),
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as _,
            size_of_val(&info) as u32,
            None,
        );
    }
    info.TotalUserTime + info.TotalKernelTime
}

#[derive(Default)]
struct Scan {
    /// Text of the first visible standard dialog (#32770: MessageBox, CRT assert, ...).
    dialog: Option<String>,
    /// Some visible window is "Not responding" (no message pumping for 5s).
    hung: bool,
}

unsafe fn scan_desktop(hdesk: HDESK) -> Scan {
    let mut scan = Scan::default();
    unsafe {
        let _ = EnumDesktopWindows(Some(hdesk), Some(on_window), LPARAM(&mut scan as *mut _ as isize));
    }
    scan
}

unsafe extern "system" fn on_window(hwnd: HWND, lp: LPARAM) -> BOOL {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let scan = &mut *(lp.0 as *mut Scan);
        scan.hung |= IsHungAppWindow(hwnd).as_bool();
        let mut cls = [0u16; 16];
        let n = GetClassNameW(hwnd, &mut cls) as usize;
        if cls[..n] != *w!("#32770").as_wide() {
            return TRUE;
        }
        let mut text = window_text(hwnd);
        let _ = EnumChildWindows(Some(hwnd), Some(on_child), LPARAM(&mut text as *mut _ as isize));
        scan.dialog = Some(text);
        FALSE
    }
}

unsafe extern "system" fn on_child(hwnd: HWND, lp: LPARAM) -> BOOL {
    let t = window_text(hwnd);
    if !t.is_empty() {
        let text = unsafe { &mut *(lp.0 as *mut String) };
        text.push_str(" | ");
        text.push_str(&t);
    }
    TRUE
}

/// WM_GETTEXT works across processes (GetWindowText doesn't for controls); timeout guards hung owners.
fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 1024];
    let mut n = 0usize;
    unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETTEXT,
            WPARAM(buf.len()),
            LPARAM(buf.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG,
            500,
            Some(&mut n),
        );
    }
    String::from_utf16_lossy(&buf[..n.min(buf.len())]).replace(['\r', '\n'], " ")
}

/// Quote one argument per the MSVC CommandLineToArgvW rules.
fn quote(arg: &OsStr, out: &mut Vec<u16>) {
    let a: Vec<u16> = arg.encode_wide().collect();
    let (bs, dq) = ('\\' as u16, '"' as u16);
    if !a.is_empty() && !a.iter().any(|&c| c == ' ' as u16 || c == '\t' as u16 || c == dq) {
        out.extend(a);
        return;
    }
    out.push(dq);
    let mut slashes = 0;
    for c in a {
        if c == bs {
            slashes += 1;
            continue;
        }
        // Backslashes are literal unless they precede a quote.
        out.extend(repeat_n(bs, if c == dq { slashes * 2 + 1 } else { slashes }));
        out.push(c);
        slashes = 0;
    }
    out.extend(repeat_n(bs, slashes * 2));
    out.push(dq);
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::CommandLineToArgvW;

    #[test]
    fn parse_args_cases() {
        let parse = |a: &[&str]| parse_args(a.iter().map(OsString::from));
        let run = |secs: u64, argv: &[&str]| {
            Ok(Cli::Run { timeout: Duration::from_secs(secs), argv: argv.iter().map(OsString::from).collect() })
        };

        assert_eq!(parse(&["a.exe"]), run(30, &["a.exe"]));
        // Flags after COMMAND belong to the command.
        assert_eq!(parse(&["--timeout", "5", "a.exe", "--timeout", "x"]), run(5, &["a.exe", "--timeout", "x"]));
        assert_eq!(parse(&["--", "--help"]), run(30, &["--help"]));
        assert_eq!(parse(&["--help"]), Ok(Cli::Help));
        for bad in [&[][..], &["--timeout"], &["--timeout", "abc", "a.exe"], &["--timeout", "-5", "a.exe"], &["--"]] {
            assert_eq!(parse(bad), Err(USAGE), "{bad:?}");
        }
    }

    #[test]
    fn hang_watch_kills_idle_hangs_only() {
        let t = Instant::now();
        let s = |n: u64| t + Duration::from_secs(n);
        let core = 10_000_000i64; // one core-second in 100ns units

        // Deadlock: hung, ~1% CPU (background threads) -> killed once HANG_LIMIT passes.
        let mut w = HangWatch::default();
        assert!(!w.stuck(true, 0, s(0)));
        assert!(!w.stuck(true, core / 10, s(59)));
        assert!(w.stuck(true, core * 6 / 10, s(60)));

        // Busy: hung, one full core -> never killed.
        let mut w = HangWatch::default();
        assert!(!w.stuck(true, 0, s(0)));
        assert!(!w.stuck(true, 60 * core, s(60)));
        assert!(!w.stuck(true, 120 * core, s(120)));

        // Recovering resets the clock.
        let mut w = HangWatch::default();
        assert!(!w.stuck(true, 0, s(0)));
        assert!(!w.stuck(false, 0, s(30)));
        assert!(!w.stuck(true, 0, s(31)));
        assert!(!w.stuck(true, 0, s(90)));
        assert!(w.stuck(true, 0, s(91)));
    }

    #[test]
    fn quote_roundtrips_through_commandlinetoargvw() {
        let cases = ["plain", "", "with space", r#"a"b"#, r"trail\", r"C:\dir with\", r#"x\"y"#, r"a\\b"];
        let mut line: Vec<u16> = "prog".encode_utf16().collect();
        for c in cases {
            line.push(' ' as u16);
            quote(OsStr::new(c), &mut line);
        }
        line.push(0);
        let mut n = 0;
        let argv = unsafe { CommandLineToArgvW(PCWSTR(line.as_ptr()), &mut n) };
        let got: Vec<String> =
            (1..n as usize).map(|i| unsafe { (*argv.add(i)).to_string().unwrap() }).collect();
        assert_eq!(got, cases);
    }
}
