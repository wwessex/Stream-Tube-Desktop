//! JavaScript challenges in YouTube stream URLs: the `n` parameter and the
//! `signatureCipher` of the TV client's formats are scrambled by YouTube's player
//! script, and a URL left unsolved is answered with 403. They are solved with
//! yt-dlp's challenge solver scripts (github.com/yt-dlp/ejs, public domain).
//! The scripts run the whole player script, so a JavaScript engine is needed:
//! Deno or Node.js when one is on the machine (a V8 start solves a new player
//! in half a second), otherwise the QuickJS engine built into the app (about
//! two seconds, once per player version; the results are cached).
use super::http;
use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const EJS_RELEASE: &str = "https://github.com/yt-dlp/ejs/releases/latest/download";
const LIB_SCRIPT: &str = "yt.solver.lib.min.js";
const CORE_SCRIPT: &str = "yt.solver.core.min.js";
/// A solve takes an external runtime about a second; a wedged one is killed after this.
const RUN_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a downloaded solver script is used before looking for a newer release.
const SCRIPT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// YouTube's current TV player script.
pub struct Player {
    pub id: String,
    pub js: String,
    /// `signatureTimestamp`: player requests without it get "The page needs to be reloaded".
    pub sts: Option<u64>,
}

static PLAYER: Mutex<Option<(Arc<Player>, Instant)>> = Mutex::new(None);

/// The current player script, cached for an hour.
pub fn player() -> Result<Arc<Player>> {
    if let Some((p, t)) = PLAYER.lock().clone() {
        if t.elapsed() < Duration::from_secs(3600) {
            return Ok(p);
        }
    }
    let api = http::get_text("https://www.youtube.com/iframe_api")?;
    let id = player_id(&api).ok_or_else(|| anyhow!("no player id in iframe_api"))?;
    let known = PLAYER.lock().clone().map(|(p, _)| p).filter(|p| p.id == id);
    let player = match known {
        Some(p) => p,
        None => {
            let js = http::get_text(&format!("https://www.youtube.com/s/player/{id}/tv-player-ias.vflset/tv-player-ias.js"))?;
            let sts = signature_timestamp(&js);
            Arc::new(Player { id, js, sts })
        }
    };
    *PLAYER.lock() = Some((player.clone(), Instant::now()));
    Ok(player)
}

/// `.../s/player/<id>/...` from the iframe API script (slashes are escaped there).
fn player_id(api: &str) -> Option<String> {
    let i = api.find("player\\/").map(|i| i + 8).or_else(|| api.find("player/").map(|i| i + 7))?;
    let id: String = api[i..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    (id.len() == 8).then_some(id)
}

fn signature_timestamp(js: &str) -> Option<u64> {
    let i = js.find("signatureTimestamp")? + "signatureTimestamp".len();
    let rest = js[i..].trim_start_matches([':', '=']);
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Deno,
    Node,
    /// QuickJS linked into the app, through `rquickjs`
    Embedded,
}

#[derive(Clone, Debug)]
pub struct Runtime {
    kind: Kind,
    path: PathBuf,
    version: (u64, u64, u64),
}

impl Runtime {
    pub fn describe(&self) -> String {
        let (a, b, c) = self.version;
        match self.kind {
            Kind::Deno => format!("Deno {a}.{b}.{c}"),
            Kind::Node => format!("Node.js {a}.{b}.{c}"),
            Kind::Embedded => "built-in QuickJS".into(),
        }
    }

    fn embedded() -> Runtime {
        Runtime { kind: Kind::Embedded, path: PathBuf::new(), version: (0, 0, 0) }
    }
}

/// yt-dlp only enables Deno by itself, and without a JavaScript runtime the
/// signed-in requests come back with no formats (#260): point it at the
/// runtime the app found.
pub fn ytdlp_runtime_args() -> Vec<std::ffi::OsString> {
    let r = runtime();
    let name = match r.kind {
        Kind::Deno => "deno",
        Kind::Node => "node",
        Kind::Embedded => return Vec::new(),
    };
    let mut spec = std::ffi::OsString::from(format!("{name}:"));
    spec.push(&r.path);
    vec!["--js-runtimes".into(), spec]
}

static RUNTIME: Mutex<Option<(Option<Runtime>, Instant)>> = Mutex::new(None);

/// The JavaScript runtime to solve with: Deno or Node.js from this machine when
/// one is found, else the built-in engine. A miss is rechecked after a minute
/// so installing one takes effect without a restart.
pub fn runtime() -> Runtime {
    if let Some((r, t)) = RUNTIME.lock().clone() {
        if r.is_some() || t.elapsed() < Duration::from_secs(60) {
            return r.unwrap_or_else(Runtime::embedded);
        }
    }
    let found = find_runtime();
    *RUNTIME.lock() = Some((found.clone(), Instant::now()));
    found.unwrap_or_else(Runtime::embedded)
}

/// Directories the runtimes are usually installed to, for when the app was
/// started with a minimal PATH (Finder, a desktop launcher).
fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = dirs::home_dir();
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(p) = std::env::var_os(var) {
                dirs.push(PathBuf::from(p).join("nodejs"));
            }
        }
        if let Some(h) = &home {
            dirs.push(h.join(".deno").join("bin"));
            dirs.push(h.join("AppData").join("Local").join("Programs").join("nodejs"));
            dirs.push(h.join("scoop").join("shims"));
        }
    } else {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from));
        if let Some(h) = &home {
            dirs.push(h.join(".deno").join("bin"));
            dirs.push(h.join(".volta").join("bin"));
            dirs.push(h.join(".local").join("bin"));
            // nvm keeps one directory per Node version
            if let Ok(rd) = std::fs::read_dir(h.join(".nvm").join("versions").join("node")) {
                let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("bin")).collect();
                v.sort();
                v.reverse();
                dirs.extend(v);
            }
        }
    }
    dirs
}

fn find_runtime() -> Option<Runtime> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(extra_dirs());
    let mut best: Option<Runtime> = None;
    for (kind, exe, min) in [(Kind::Deno, "deno", (2, 3, 0)), (Kind::Node, "node", (22, 0, 0))] {
        for dir in &dirs {
            let mut path = dir.join(exe);
            if cfg!(windows) {
                path.set_extension("exe");
            }
            if !path.is_file() {
                continue;
            }
            let Some(version) = probe_version(&path) else { continue };
            if version < min {
                continue;
            }
            let r = Runtime { kind, path, version };
            // Deno is preferred: it needs no permission flags and starts faster
            if best.as_ref().is_none_or(|b| b.kind == Kind::Node && kind == Kind::Deno) {
                best = Some(r);
            }
            break;
        }
    }
    best
}

fn probe_version(path: &std::path::Path) -> Option<(u64, u64, u64)> {
    let mut cmd = Command::new(path);
    cmd.arg("--version").stdin(Stdio::null());
    crate::api::streams::hide_window(&mut cmd);
    let out = cmd.output().ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// The first `a.b.c` in text such as `deno 2.9.8 (stable, ...)` or `v24.20.0`.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split(|c: char| c.is_whitespace() || c == 'v' || c == '(' || c == ')').find_map(|tok| {
        let mut it = tok.split('.').map(|p| p.parse::<u64>().ok());
        match (it.next(), it.next(), it.next()) {
            (Some(Some(a)), Some(Some(b)), Some(Some(c))) => Some((a, b, c)),
            _ => None,
        }
    })
}

/// The solver's lib and core scripts.
type Scripts = Arc<(String, String)>;

static SCRIPTS: Mutex<Option<(Scripts, Instant)>> = Mutex::new(None);

/// The solver's lib and core scripts: from the data directory when they are
/// less than a week old, otherwise the latest release (the old copy is kept
/// when the download fails).
fn scripts() -> Result<Scripts> {
    if let Some((s, t)) = SCRIPTS.lock().clone() {
        if t.elapsed() < Duration::from_secs(24 * 3600) {
            return Ok(s);
        }
    }
    let dir = crate::store::data_dir().join("ejs");
    let _ = std::fs::create_dir_all(&dir);
    let load = |name: &str| -> Result<String> {
        let path = dir.join(name);
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age < SCRIPT_MAX_AGE);
        if fresh {
            if let Ok(s) = std::fs::read_to_string(&path) {
                return Ok(s);
            }
        }
        match http::get_text(&format!("{EJS_RELEASE}/{name}")) {
            Ok(code) if code.contains("jsc") || code.len() > 1000 => {
                let _ = std::fs::write(&path, &code);
                Ok(code)
            }
            Ok(_) => Err(anyhow!("unexpected content for {name}")),
            Err(e) => std::fs::read_to_string(&path).map_err(|_| e).context(format!("downloading {name}")),
        }
    };
    let s = Arc::new((load(LIB_SCRIPT)?, load(CORE_SCRIPT)?));
    *SCRIPTS.lock() = Some((s.clone(), Instant::now()));
    Ok(s)
}

#[derive(Default, Debug)]
pub struct Solved {
    pub n: HashMap<String, String>,
    pub sig: HashMap<String, String>,
}

/// Solved challenges by (player id, is signature, challenge).
type SolvedCache = Mutex<HashMap<(String, bool, String), String>>;

fn solved_cache() -> &'static SolvedCache {
    static C: OnceLock<SolvedCache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Solves `n` and signature challenges against the given player. Results are
/// cached per player, so a video whose challenges were seen before costs no
/// process start.
pub fn solve(player: &Player, n: &[String], sig: &[String]) -> Result<Solved> {
    let mut out = Solved::default();
    let (mut todo_n, mut todo_sig) = (Vec::new(), Vec::new());
    {
        let cache = solved_cache().lock();
        for c in n {
            match cache.get(&(player.id.clone(), false, c.clone())) {
                Some(v) => {
                    out.n.insert(c.clone(), v.clone());
                }
                None => todo_n.push(c.clone()),
            }
        }
        for c in sig {
            match cache.get(&(player.id.clone(), true, c.clone())) {
                Some(v) => {
                    out.sig.insert(c.clone(), v.clone());
                }
                None => todo_sig.push(c.clone()),
            }
        }
    }
    todo_n.dedup();
    todo_sig.dedup();
    if todo_n.is_empty() && todo_sig.is_empty() {
        return Ok(out);
    }
    let rt = runtime();
    let scripts = scripts()?;
    let mut requests = Vec::new();
    if !todo_n.is_empty() {
        requests.push(json!({ "type": "n", "challenges": todo_n }));
    }
    if !todo_sig.is_empty() {
        requests.push(json!({ "type": "sig", "challenges": todo_sig }));
    }
    let data = json!({ "type": "player", "player": player.js, "requests": requests, "output_preprocessed": false });
    let program = format!(
        "{}\nObject.assign(globalThis, lib);\n{}\nconsole.log(JSON.stringify(jsc({})));\n",
        scripts.0,
        scripts.1,
        serde_json::to_string(&data)?
    );
    let t = Instant::now();
    let stdout = run(&rt, program)?;
    let v: Value = serde_json::from_str(stdout.trim()).context("solver output is not JSON")?;
    if v.get("type").and_then(Value::as_str) == Some("error") {
        return Err(anyhow!("solver: {}", v.get("error").and_then(Value::as_str).unwrap_or("unknown error")));
    }
    let responses = v.get("responses").and_then(Value::as_array).ok_or_else(|| anyhow!("solver: no responses"))?;
    let mut cache = solved_cache().lock();
    for (req, resp) in requests.iter().zip(responses) {
        let is_sig = req["type"] == "sig";
        if resp.get("type").and_then(Value::as_str) == Some("error") {
            if crate::perf::on() {
                eprintln!("[jsc] {} challenge failed: {}", if is_sig { "sig" } else { "n" }, resp.get("error").and_then(Value::as_str).unwrap_or(""));
            }
            continue;
        }
        for (challenge, solved) in resp.get("data").and_then(Value::as_object).into_iter().flatten() {
            let Some(solved) = solved.as_str() else { continue };
            cache.insert((player.id.clone(), is_sig, challenge.clone()), solved.to_string());
            if is_sig { &mut out.sig } else { &mut out.n }.insert(challenge.clone(), solved.to_string());
        }
    }
    if crate::perf::on() {
        eprintln!("[jsc] {} solved {} n + {} sig challenges in {:?}", rt.describe(), todo_n.len(), todo_sig.len(), t.elapsed());
    }
    Ok(out)
}

/// Runs the program in the runtime, returning what it logged to the console.
fn run(rt: &Runtime, program: String) -> Result<String> {
    if rt.kind == Kind::Embedded {
        return run_embedded(program);
    }
    let mut cmd = Command::new(&rt.path);
    match rt.kind {
        Kind::Deno => {
            cmd.args(["run", "--ext=js", "--no-code-cache", "--no-prompt", "--no-remote", "--no-lock", "--node-modules-dir=none", "--no-config", "-"]);
        }
        Kind::Node => {
            // the permission flag left experimental status in Node 23.5
            if rt.version >= (23, 5, 0) {
                cmd.arg("--permission");
            } else {
                cmd.args(["--experimental-permission", "--no-warnings=ExperimentalWarning"]);
            }
            cmd.arg("-");
        }
        Kind::Embedded => unreachable!(),
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::api::streams::hide_window(&mut cmd);
    let mut child = cmd.spawn().with_context(|| format!("starting {}", rt.describe()))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(program.as_bytes());
    });
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    let out_reader = std::thread::spawn(move || read_all(stdout.as_mut()));
    let err_reader = std::thread::spawn(move || read_all(stderr.as_mut()));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > RUN_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("{} did not finish in {:?}", rt.describe(), RUN_TIMEOUT));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = writer.join();
    let stdout = out_reader.join().unwrap_or_default();
    if !status.success() {
        let err = err_reader.join().unwrap_or_default();
        let line = err.lines().find(|l| l.contains("rror")).or_else(|| err.lines().next()).unwrap_or("").trim();
        return Err(anyhow!("{} failed: {line}", rt.describe()));
    }
    Ok(stdout)
}

fn read_all(pipe: Option<&mut impl std::io::Read>) -> String {
    let mut buf = Vec::new();
    if let Some(p) = pipe {
        let _ = p.read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Runs the program in the built-in QuickJS engine. The player script recurses
/// deeply, so it gets its own thread with a large stack; `console.log` is the
/// one browser API the solver's output needs.
fn run_embedded(program: String) -> Result<String> {
    use rquickjs::{Context, Function, Object, Runtime};
    let worker = std::thread::Builder::new().name("quickjs".into()).stack_size(64 << 20).spawn(move || -> Result<String> {
        let rt = Runtime::new().context("QuickJS runtime")?;
        rt.set_max_stack_size(48 << 20);
        let ctx = Context::full(&rt).context("QuickJS context")?;
        ctx.with(|ctx| {
            let logged = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
            let console = Object::new(ctx.clone())?;
            let sink = logged.clone();
            console.set(
                "log",
                Function::new(ctx.clone(), move |line: String| {
                    let mut s = sink.borrow_mut();
                    s.push_str(&line);
                    s.push('\n');
                })?,
            )?;
            ctx.globals().set("console", console)?;
            if let Err(e) = ctx.eval::<(), _>(program.as_bytes()) {
                let detail = match ctx.catch().into_exception() {
                    Some(exc) => exc.message().unwrap_or_default(),
                    None => String::new(),
                };
                return Err(anyhow!("QuickJS failed: {e} {detail}"));
            }
            Ok(logged.take())
        })
    })?;
    worker.join().map_err(|_| anyhow!("QuickJS thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_player_id_and_sts() {
        let api = r"var scriptUrl = 'https:\/\/www.youtube.com\/s\/player\/b485e332\/www-widgetapi.vflset\/www-widgetapi.js';";
        assert_eq!(player_id(api).as_deref(), Some("b485e332"));
        assert_eq!(player_id("https://www.youtube.com/s/player/0123abcd/base.js").as_deref(), Some("0123abcd"));
        assert_eq!(player_id("nothing here"), None);
        assert_eq!(signature_timestamp("a={signatureTimestamp:20779,b:1}"), Some(20779));
        assert_eq!(signature_timestamp("signatureTimestamp=20780&x"), Some(20780));
        assert_eq!(signature_timestamp("no sts"), None);
    }

    #[test]
    fn parses_runtime_versions() {
        assert_eq!(parse_version("deno 2.9.8 (stable, release, aarch64-apple-darwin)\nv8 14.0\n"), Some((2, 9, 8)));
        assert_eq!(parse_version("v24.20.0\n"), Some((24, 20, 0)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn embedded_engine_runs_a_program() {
        let out = run_embedded("var a = [3, 1, 2]; a.sort(); console.log(JSON.stringify({ a: a, s: 'x'.padStart(3, 'y') }));".into()).unwrap();
        assert_eq!(out.trim(), r#"{"a":[1,2,3],"s":"yyx"}"#);
        let err = run_embedded("null.x".into()).unwrap_err().to_string();
        assert!(err.contains("QuickJS failed"), "{err}");
    }

    #[test]
    #[ignore = "needs network"]
    fn solves_live_challenges() {
        let p = player().unwrap();
        println!("player {} sts {:?} runtime {}", p.id, p.sts, runtime().describe());
        let s = solve(&p, &["Ah-_xXRqTGCumKp9a".into()], &[]).unwrap();
        println!("{s:?}");
        assert_eq!(s.n.len(), 1);
    }

    /// The built-in engine against the live player, whatever else is installed.
    #[test]
    #[ignore = "needs network; takes a couple of seconds"]
    fn embedded_engine_solves_live_challenges() {
        let p = player().unwrap();
        let scripts = scripts().unwrap();
        let data = json!({ "type": "player", "player": p.js, "requests": [{ "type": "n", "challenges": ["Ah-_xXRqTGCumKp9a"] }], "output_preprocessed": false });
        let program = format!("{}\nObject.assign(globalThis, lib);\n{}\nconsole.log(JSON.stringify(jsc({})));\n", scripts.0, scripts.1, data);
        let t = Instant::now();
        let out = run_embedded(program).unwrap();
        println!("{out} in {:?}", t.elapsed());
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["responses"][0]["type"], "result");
    }
}
