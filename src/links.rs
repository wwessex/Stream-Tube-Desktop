//! A single running instance: a second launch (e.g. with a YouTube link on the
//! command line) hands its argument to the first and exits.
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// Turns a `streamtube:` link back into the YouTube URL it wraps.
/// Accepts `streamtube://open?url=<encoded>`, `streamtube:https://…` and
/// `streamtube://www.youtube.com/watch?v=…`; anything else is returned unchanged.
pub fn normalize(arg: &str) -> String {
    let a = arg.trim().trim_matches('"');
    let Some(rest) = a.strip_prefix("streamtube:").or_else(|| a.strip_prefix("STREAMTUBE:")) else {
        return a.to_string();
    };
    let rest = rest.trim_start_matches('/');
    if let Some(q) = rest.strip_prefix("open?").or_else(|| rest.strip_prefix("open/?")) {
        if let Some(u) = q.split('&').find_map(|kv| kv.strip_prefix("url=")) {
            return crate::api::http::urldecode(u);
        }
        if let Some(v) = q.split('&').find_map(|kv| kv.strip_prefix("v=")) {
            return format!("https://www.youtube.com/watch?v={v}");
        }
    }
    if rest.starts_with("http:") || rest.starts_with("https:") {
        // "https:/www…" when a browser collapsed the slashes
        let (scheme, tail) = rest.split_once(':').unwrap_or(("https", ""));
        return format!("{scheme}://{}", tail.trim_start_matches('/'));
    }
    format!("https://{rest}")
}

fn port_file() -> std::path::PathBuf {
    crate::store::data_dir().join("instance")
}

/// Whether another copy of the app is running, going by the file it wrote at start.
pub fn other_instance_running() -> bool {
    let Ok(s) = std::fs::read_to_string(port_file()) else { return false };
    let pid = s.split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok());
    pid.is_some_and(|p| p != std::process::id() && process_alive(p))
}

/// Hands `arg` to an already running instance. True when one took it (this process should exit).
pub fn forward_to_running(arg: &str) -> bool {
    let Ok(s) = std::fs::read_to_string(port_file()) else { return false };
    let mut parts = s.split_whitespace();
    let (Some(port), pid) = (parts.next().and_then(|p| p.parse::<u16>().ok()), parts.next().and_then(|p| p.parse::<u32>().ok())) else {
        return false;
    };
    // the running instance only takes links that carry the secret from this file
    let token = parts.next().unwrap_or_default();
    // Left over from an instance that has exited: connecting to its closed port
    // only fails after the timeout on Windows, which delayed every launch.
    if pid.is_some_and(|p| !process_alive(p)) {
        return false;
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    #[cfg(windows)]
    if let Some(pid) = pid {
        // let the running instance bring its window to the front
        #[link(name = "user32")]
        unsafe extern "system" {
            fn AllowSetForegroundWindow(pid: u32) -> i32;
        }
        unsafe { AllowSetForegroundWindow(pid) };
    }
    #[cfg(not(windows))]
    let _ = pid;
    if stream.write_all(format!("STREAMTUBE {token} {}\n", arg.replace('\n', " ")).as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).is_ok() && reply.trim() == "ok"
}

/// Makes `streamtube://` links open this app when nothing handles them yet (the
/// portable zip; the installer registers them itself). Runs in the background.
pub fn register_scheme() {
    #[cfg(windows)]
    std::thread::spawn(|| {
        use std::process::Command;
        let Ok(exe) = std::env::current_exe() else { return };
        let key = r"HKCU\Software\Classes\streamtube";
        let reg = |args: &[&str]| {
            let mut c = Command::new("reg");
            c.args(args);
            crate::api::streams::hide_window(&mut c);
            c.output().map(|o| o.status.success()).unwrap_or(false)
        };
        if reg(&["query", &format!(r"{key}\shell\open\command"), "/ve"]) {
            return; // registered already (by the installer or an earlier run)
        }
        let exe = exe.to_string_lossy();
        let cmd = format!("\"{exe}\" \"%1\"");
        let _ = reg(&["add", key, "/ve", "/d", "URL:Stream Tube", "/f"])
            && reg(&["add", key, "/v", "URL Protocol", "/d", "", "/f"])
            && reg(&["add", &format!(r"{key}\DefaultIcon"), "/ve", "/d", &format!("{exe},0"), "/f"])
            && reg(&["add", &format!(r"{key}\shell\open\command"), "/ve", "/d", &cmd, "/f"]);
    });
}

/// Whether a process with this id is running.
pub(crate) fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn GetExitCodeProcess(process: isize, code: *mut u32) -> i32;
            fn CloseHandle(handle: isize) -> i32;
        }
        // SAFETY: plain Win32 calls on a handle we open and close here
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h == 0 {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            !ok || code == STILL_ACTIVE
        }
    }
    #[cfg(target_os = "macos")]
    {
        let Ok(pid) = i32::try_from(pid) else { return false };
        // SAFETY: signal 0 queries a positive PID without sending a signal.
        pid > 0 && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists() || !std::path::Path::new("/proc/self").exists()
    }
}

/// Listens for links from later launches; each arrives on the returned channel.
pub fn listen(tx: futures::channel::mpsc::UnboundedSender<String>) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    // Any local process can reach a loopback port, so a link is only taken with this
    // secret. It lives in the profile folder, which only this user can read.
    let token = new_token();
    std::fs::write(port_file(), format!("{port} {} {token}", std::process::id()))?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            // a link is short; don't let another local process feed us endless bytes
            let mut reader = BufReader::new(std::io::Read::take(&stream, 16 * 1024));
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            // only our own launches speak this, and know the secret
            let Some(arg) = parse_request(&line, &token) else { continue };
            let _ = (&stream).write_all(b"ok\n");
            if tx.unbounded_send(arg.to_string()).is_err() {
                break;
            }
        }
    });
    Ok(())
}

fn parse_request<'a>(line: &'a str, token: &str) -> Option<&'a str> {
    let payload = line.trim_end_matches(['\r', '\n']).strip_prefix("STREAMTUBE ")?;
    check_token(payload, token)
}

/// A random secret for the loopback link port.
fn new_token() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..32).map(|_| format!("{:02x}", rng.random::<u8>())).collect()
}

/// The link after "<token> " when the token matches (compared in constant time).
fn check_token<'a>(rest: &'a str, token: &str) -> Option<&'a str> {
    let (given, arg) = rest.split_once(' ')?;
    let same = given.len() == token.len() && given.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    same.then_some(arg)
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn links_need_the_token() {
        let token = super::new_token();
        assert_eq!(token.len(), 64);
        assert_eq!(super::check_token(&format!("{token} https://youtu.be/abc"), &token), Some("https://youtu.be/abc"));
        assert_eq!(super::check_token("https://youtu.be/abc", &token), None);
        assert_eq!(super::check_token(&format!("{} https://youtu.be/abc", "0".repeat(64)), &token), None);
        assert_ne!(super::new_token(), token);
    }

    #[test]
    fn launch_without_a_url_is_forwarded_to_the_running_instance() {
        let token = super::new_token();
        assert_eq!(super::parse_request(&format!("STREAMTUBE {token} \n"), &token), Some(""));
        assert_eq!(super::parse_request(&format!("STREAMTUBE {token}\n"), &token), None);
    }

    #[test]
    fn alive() {
        assert!(super::process_alive(std::process::id()));
        assert!(!super::process_alive(u32::MAX - 1));
    }

    #[test]
    fn scheme_links() {
        assert_eq!(normalize("streamtube://open?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Dabc"), "https://www.youtube.com/watch?v=abc");
        assert_eq!(normalize("streamtube:https://youtu.be/abc"), "https://youtu.be/abc");
        assert_eq!(normalize("streamtube://https:/youtu.be/abc"), "https://youtu.be/abc");
        assert_eq!(normalize("streamtube://www.youtube.com/watch?v=abc"), "https://www.youtube.com/watch?v=abc");
        assert_eq!(normalize("https://youtu.be/abc"), "https://youtu.be/abc");
    }
}
