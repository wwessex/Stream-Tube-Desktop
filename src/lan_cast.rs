use crate::ui::app::{Route, navigate, restore_window, route_from_link};
use futures::{StreamExt, channel::mpsc};
use gpui_kit::{AnyWindowHandle, App, Global};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

const MAX_REQUEST_BYTES: u64 = 8192;

#[derive(Default)]
struct Casting(Option<Receiver>);
impl Global for Casting {}

struct Receiver {
    stopped: Arc<AtomicBool>,
    url: String,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn url(cx: &App) -> Option<&str> {
    cx.try_global::<Casting>()?.0.as_ref().map(|receiver| receiver.url.as_str())
}

pub(crate) fn stop(cx: &mut App) {
    cx.set_global(Casting(None));
    cx.refresh_windows();
}

pub(crate) fn addresses() -> std::io::Result<Vec<Ipv4Addr>> {
    let mut addresses: Vec<_> = if_addrs::get_if_addrs()?.into_iter()
        .filter(|interface| interface.is_oper_up() && !interface.is_p2p())
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(ip) if ip.is_private() || ip.is_link_local() => Some(ip),
            _ => None,
        }).collect();
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

pub(crate) fn start(address: Ipv4Addr, window: AnyWindowHandle, cx: &mut App) -> anyhow::Result<()> {
    if url(cx).is_some() {
        return Ok(());
    }
    anyhow::ensure!(address.is_private() || address.is_link_local(), "No private LAN address");
    let listener = TcpListener::bind((address, 0))?;
    listener.set_nonblocking(true)?;
    let token: [u8; 24] = rand::random();
    let secret = token.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let local = format!("http://{}/{}", listener.local_addr()?, secret);
    let stopped = Arc::new(AtomicBool::new(false));
    let shutdown = stopped.clone();
    let (send, mut incoming) = mpsc::channel(8);
    std::thread::Builder::new().name("lan-cast".into()).spawn(move || {
        let mut send = send;
        while !shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = reply(stream, &secret, |route| send.try_send(route).is_ok());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
    })?;
    cx.set_global(Casting(Some(Receiver { stopped: stopped.clone(), url: local })));
    cx.spawn(async move |cx| {
        while let Some(route) = incoming.next().await {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            let _ = cx.update(|cx| window.update(cx, |_, window, cx| {
                restore_window(window);
                window.activate_window();
                navigate(route, window, cx);
            }));
        }
    }).detach();
    cx.refresh_windows();
    Ok(())
}

fn video_route(input: &str) -> Option<Route> {
    let input = input.trim();
    let uri: ureq::http::Uri = input.parse().ok()?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return None;
    }
    if !matches!(uri.host()?.to_ascii_lowercase().as_str(), "youtu.be" | "youtube.com" | "www.youtube.com" | "m.youtube.com") {
        return None;
    }
    if uri.authority()?.as_str().contains('@') {
        return None;
    }
    route_from_link(input).filter(|route| matches!(route, Route::Watch { .. }))
}

struct RequestReader<'a> {
    socket: &'a mut TcpStream,
    deadline: Instant,
}

impl Read for RequestReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.socket.set_read_timeout(Some(left))?;
        self.socket.read(buffer)
    }
}

const FORM: &str = "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
    <title>Stream Tube</title><main><h1>Stream Tube</h1><form method=post>\
    <label>YouTube link <input name=url type=url required></label> <button>Play</button></form></main>";

fn reply(mut socket: TcpStream, token: &str, accept: impl FnOnce(Route) -> bool) -> std::io::Result<()> {
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut request = BufReader::new(RequestReader {
        socket: &mut socket,
        deadline: Instant::now() + Duration::from_secs(2),
    }).take(MAX_REQUEST_BYTES);
    let mut line = String::new();
    request.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default();
    let expected = format!("/{token}");
    let valid = path.len() == expected.len()
        && path.bytes().zip(expected.bytes()).fold(0, |difference, (a, b)| difference | (a ^ b)) == 0;
    if !valid {
        return send(&mut socket, "404 Not Found", "Not found");
    }
    let mut length = None;
    loop {
        line.clear();
        if request.read_line(&mut line)? == 0 {
            return send(&mut socket, "400 Bad Request", "Incomplete request");
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("transfer-encoding") {
                return send(&mut socket, "400 Bad Request", "Unsupported request");
            }
            if name.eq_ignore_ascii_case("content-length") {
                let parsed = value.trim().parse::<usize>();
                if length.is_some() || parsed.is_err() {
                    return send(&mut socket, "400 Bad Request", "Invalid request length");
                }
                length = parsed.ok();
            }
        }
    }
    match method.as_str() {
        "GET" => send(&mut socket, "200 OK", FORM),
        "POST" => {
            let Some(length) = length.filter(|&length| (1..=4096).contains(&length)) else {
                return send(&mut socket, "400 Bad Request", "Invalid request length");
            };
            let mut body = vec![0; length];
            request.read_exact(&mut body)?;
            let route = std::str::from_utf8(&body).ok()
                .and_then(|body| body.strip_prefix("url="))
                .and_then(|encoded| video_route(&crate::api::http::urldecode(&encoded.replace('+', " "))));
            let Some(route) = route else {
                return send(&mut socket, "400 Bad Request", "Enter a YouTube video link.");
            };
            if accept(route) {
                send(&mut socket, "200 OK", &format!("{FORM}<p>Link sent to Stream Tube.</p>"))
            } else {
                send(&mut socket, "503 Service Unavailable", "Stream Tube is busy.")
            }
        }
        _ => send(&mut socket, "405 Method Not Allowed", "Method not allowed"),
    }
}

fn send(socket: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    write!(socket, "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; form-action 'self'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}", body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(request: &str) -> (String, Option<Route>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        let task = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut route = None;
            reply(socket, "test-token", |video| {
                route = Some(video);
                true
            }).unwrap();
            route
        });
        client.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        (response, task.join().unwrap())
    }

    #[test]
    fn accepts_links_from_the_private_cast_form() {
        let (page, route) = exchange("GET /test-token HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(page.starts_with("HTTP/1.1 200"));
        assert!(page.contains("<form method=post>"));
        assert!(route.is_none());

        let body = "url=https%3A%2F%2Fyoutu.be%2FdQw4w9WgXcQ";
        let post = |path| format!("POST /{path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let (valid, route) = exchange(&post("test-token"));
        assert!(valid.starts_with("HTTP/1.1 200"));
        assert!(matches!(route, Some(Route::Watch { id, .. }) if id == "dQw4w9WgXcQ"));
        let (invalid, route) = exchange(&post("bad-token"));
        assert!(invalid.starts_with("HTTP/1.1 404"));
        assert!(route.is_none());
    }

    #[test]
    fn casting_only_accepts_youtube_video_urls() {
        assert!(video_route("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=15").is_some());
        assert!(video_route("https://youtu.be/dQw4w9WgXcQ").is_some());
        assert!(video_route("https://youtube.com.example.org/watch?v=dQw4w9WgXcQ").is_none());
        assert!(video_route("streamtube://open?url=https://youtu.be/dQw4w9WgXcQ").is_none());
        assert!(video_route("https://www.youtube.com/playlist?list=PLExample").is_none());
    }
}
