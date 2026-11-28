//! A single, user-initiated connection from our Safari extension.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use objc2::{msg_send, runtime::AnyClass};
use objc2_foundation::{NSBundle, NSDictionary, NSString};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::OnceLock,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const EXTENSION_BUNDLE: &str = "Stream Tube Safari History Extension.appex";
const TEST_EXTENSION_PREFIX: &str = "io.github.avaneesh-inamdar.StreamTube.SafariPrototype.Test.";
const MAX_PAYLOAD: u64 = 16_384;
const TIMEOUT: Duration = Duration::from_secs(300);
const PURPOSE: &str = "history-and-reactions";

#[link(name = "SafariServices", kind = "framework")]
unsafe extern "C" {}

fn extension_identifier() -> Option<&'static str> {
    static IDENTIFIER: OnceLock<Option<String>> = OnceLock::new();
    IDENTIFIER
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            let contents = exe.parent()?.parent()?;
            let extension = contents.join("PlugIns").join(EXTENSION_BUNDLE);
            let path = NSString::from_str(extension.to_str()?);
            NSBundle::bundleWithPath(&path)?.bundleIdentifier().map(|identifier| identifier.to_string())
        })
        .as_deref()
}

pub fn available() -> bool {
    extension_identifier().is_some()
}

pub(crate) fn profile_name() -> Option<String> {
    extension_identifier().map(profile_name_for_extension)
}

fn profile_name_for_extension(identifier: &str) -> String {
    let test_id = identifier.strip_prefix(TEST_EXTENSION_PREFIX)
        .and_then(|suffix| suffix.strip_suffix(".Extension"))
        .filter(|id| {
            !id.is_empty() && id.len() <= 32 && !id.starts_with('-') && !id.ends_with('-')
                && id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        });
    match test_id {
        Some(id) => format!("StreamTube Safari Test ({id})"),
        None => "StreamTube Safari Prototype".into(),
    }
}

#[derive(Clone)]
pub struct Request {
    token: String,
    port: u16,
}

impl Request {
    /// Called on the main thread while the in-app connection dialog is open.
    pub fn notify_extension(&self) {
        let Some(identifier) = extension_identifier() else {
            return;
        };
        let Some(application) = AnyClass::get(c"SFSafariApplication") else {
            return;
        };
        let name = NSString::from_str("watch-history-request");
        let identifier = NSString::from_str(identifier);
        let token_key = NSString::from_str("token");
        let port_key = NSString::from_str("port");
        let purpose_key = NSString::from_str("purpose");
        let token = NSString::from_str(&self.token);
        let port = NSString::from_str(&self.port.to_string());
        let purpose = NSString::from_str(PURPOSE);
        let payload = NSDictionary::from_slices(&[&*token_key, &*port_key, &*purpose_key], &[&*token, &*port, &*purpose]);
        // SAFETY: These objects and the optional nil completion match the SafariServices API.
        unsafe {
            let _: () = msg_send![
                application,
                dispatchMessageWithName: &*name,
                toExtensionWithIdentifier: &*identifier,
                userInfo: &*payload,
                completionHandler: std::ptr::null::<std::ffi::c_void>()
            ];
        }
    }
}

pub struct Pending {
    listener: TcpListener,
    request: Request,
}

impl Pending {
    pub(crate) fn new() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            request: Request { token, port },
        })
    }

    pub fn request(&self) -> Request {
        self.request.clone()
    }

    pub fn open_safari() -> Result<()> {
        let status = std::process::Command::new("/usr/bin/open")
            .args(["-b", "com.apple.Safari", "https://www.youtube.com/"])
            .status()
            .context("Could not open Safari")?;
        if !status.success() {
            bail!("Could not open Safari");
        }
        Ok(())
    }

    pub fn wait(self, cancel: &AtomicBool) -> Result<String> {
        let deadline = Instant::now() + TIMEOUT;
        while !cancel.load(Ordering::Acquire) && Instant::now() < deadline {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    let Some(cookies) = read_session(&stream, &self.request) else {
                        continue;
                    };
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    let _ = stream.write_all(&[1]);
                    drop(self.listener);
                    return super::web::connect_safari(&cookies, cancel);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(80));
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!(
            "Safari connection ended. Open the extension on a signed-in YouTube tab and try again."
        )
    }
}

#[derive(serde::Deserialize)]
struct Handoff {
    purpose: String,
    cookies: Vec<Value>,
}

fn read_session(stream: &TcpStream, request: &Request) -> Option<Vec<Value>> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .ok()?;
    let mut reader = BufReader::new(stream);
    let mut challenge = [0_u8; 44];
    reader.read_exact(&mut challenge).ok()?;
    let (terminator, presented) = challenge.split_last()?;
    let mismatch = presented
        .iter()
        .zip(request.token.bytes())
        .fold(0, |acc, (a, b)| acc | (a ^ b));
    if mismatch != 0 || *terminator != b'\n' {
        return None;
    }
    let mut payload = Vec::new();
    reader
        .take(MAX_PAYLOAD + 1)
        .read_until(b'\n', &mut payload)
        .ok()?;
    if payload.last() != Some(&b'\n') {
        return None;
    }
    // The old watch-history-only popup sends an array without consent for reactions.
    let handoff: Handoff = serde_json::from_slice(&payload).ok()?;
    (handoff.purpose == PURPOSE).then_some(handoff.cookies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extension_identifiers_select_separate_profiles() {
        assert_eq!(
            profile_name_for_extension("io.github.avaneesh-inamdar.StreamTube.SafariPrototype.Extension"),
            "StreamTube Safari Prototype"
        );
        assert_eq!(
            profile_name_for_extension(&format!("{TEST_EXTENSION_PREFIX}resume-check.Extension")),
            "StreamTube Safari Test (resume-check)"
        );
        assert_eq!(
            profile_name_for_extension(&format!("{TEST_EXTENSION_PREFIX}pip.Extension")),
            "StreamTube Safari Test (pip)"
        );
        assert_eq!(
            profile_name_for_extension(&format!("{TEST_EXTENSION_PREFIX}../outside.Extension")),
            "StreamTube Safari Prototype"
        );
    }

    #[test]
    fn handoff_requires_the_current_token() {
        let pending = Pending::new().unwrap();
        pending.listener.set_nonblocking(false).unwrap();
        let correct = pending.request();
        let other = Pending::new().unwrap().request();
        let mut send = TcpStream::connect(("127.0.0.1", correct.port)).unwrap();
        let (received, _) = pending.listener.accept().unwrap();
        let cookies = json!([{"name": "TEST", "domain": ".youtube.com", "value": "synthetic"}]);
        let payload = json!({ "purpose": PURPOSE, "cookies": cookies });
        writeln!(send, "{}", other.token).unwrap();
        writeln!(send, "{payload}").unwrap();
        assert!(read_session(&received, &correct).is_none());

        let mut send = TcpStream::connect(("127.0.0.1", correct.port)).unwrap();
        let (received, _) = pending.listener.accept().unwrap();
        writeln!(send, "{}", correct.token).unwrap();
        writeln!(send, "{payload}").unwrap();
        assert_eq!(
            json!(read_session(&received, &correct).unwrap()),
            cookies
        );
    }

    #[test]
    fn handoff_rejects_an_incomplete_or_oversized_payload() {
        for size in [8, MAX_PAYLOAD as usize + 1] {
            let pending = Pending::new().unwrap();
            pending.listener.set_nonblocking(false).unwrap();
            let mut send = TcpStream::connect(("127.0.0.1", pending.request.port)).unwrap();
            let (received, _) = pending.listener.accept().unwrap();
            writeln!(send, "{}", pending.request.token).unwrap();
            send.write_all(&vec![b'a'; size]).unwrap();
            drop(send);
            assert!(read_session(&received, &pending.request).is_none());
        }
    }

    #[test]
    fn handoff_requires_consent_for_history_and_reactions() {
        let pending = Pending::new().unwrap();
        pending.listener.set_nonblocking(false).unwrap();
        let cookies = json!([{"name": "TEST", "domain": ".youtube.com", "value": "synthetic"}]);
        for (payload, accepted) in [
            (cookies.clone(), false),
            (json!({ "purpose": "watch-history", "cookies": cookies }), false),
            (json!({ "purpose": "comment-reactions", "cookies": cookies }), false),
            (json!({ "purpose": PURPOSE, "cookies": cookies }), true),
        ] {
            let mut send = TcpStream::connect(("127.0.0.1", pending.request.port)).unwrap();
            let (received, _) = pending.listener.accept().unwrap();
            writeln!(send, "{}\n{payload}", pending.request.token).unwrap();
            let result = read_session(&received, &pending.request);
            assert_eq!(result.is_some(), accepted);
            if let Some(cookies_read) = result {
                assert_eq!(json!(cookies_read), cookies);
            }
        }
    }
}
