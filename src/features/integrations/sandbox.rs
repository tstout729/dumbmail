//! Runs an integration's translation script in QuickJS, in its own short-lived OS process.
//!
//! The server re-executes its own binary (`/proc/self/exe`) with the hidden `sandbox-run`
//! subcommand. The child gets an empty environment (no secrets, no `DATABASE_URL`), reads the
//! script and its input on stdin, runs QuickJS with an engine memory cap, a stack cap and an
//! interrupt deadline, and writes the result to stdout. It never opens a file, a network or a
//! database connection. Before the child starts, the parent sets hard resource limits on it
//! (address space, CPU seconds, no core file, no file writes, a handful of file descriptors) and
//! asks the kernel to kill it if the server goes away. The parent then:
//!
//! - kills the child's whole process group (SIGKILL) at a hard wall-clock limit, so a native loop
//!   in the engine that never polls the interrupt (a huge sort, `repeat`, `JSON.stringify`) can't
//!   hang a worker;
//! - stops reading stdout (and kills the child) the moment it passes the output cap, while reading;
//! - always reaps the child, so nothing is left behind, even when the caller gives up.
//!
//! A runaway or hostile script can only kill its own process. Admission (how many scripts run at
//! once, per account and in total) is in [`Slots`]; callers that can't get a slot put their job back.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use once_cell::sync::{Lazy, OnceCell};
use rquickjs::{Context, Function, Runtime};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

/// The hidden subcommand the server binary answers with a sandboxed run.
pub const SUBCOMMAND: &str = "sandbox-run";
/// Largest script accepted.
pub const SCRIPT_MAX_BYTES: usize = 256 * 1024;
/// Largest input (payload, headers, settings and state) passed to a script.
pub const INPUT_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Largest result (the JSON text of the returned actions).
pub const OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// Memory the engine may allocate in one run, input included.
pub const MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// JavaScript stack one run may use.
pub const STACK_LIMIT_BYTES: usize = 512 * 1024;
/// Wall-clock time one run may take before the engine interrupts it.
pub const TIME_LIMIT: Duration = Duration::from_secs(2);
/// Extra time the parent waits after the engine's own deadline before killing the process.
const KILL_GRACE: Duration = Duration::from_millis(750);
/// Address space the child process may map (the binary, the engine heap and some slack).
const CHILD_ADDRESS_SPACE: u64 = 1024 * 1024 * 1024;

// ---------- Admission ----------

/// Scripts running at once: at most [`PER_ACCOUNT`] per account and a global cap (default 8, set
/// with `INTEGRATION_SCRIPT_SLOTS`). A caller that can't get a slot doesn't wait on a worker.
pub struct Slots;

pub const PER_ACCOUNT: usize = 2;

struct SlotState {
    total: usize,
    by_account: HashMap<Uuid, usize>,
}

static SLOT_STATE: Lazy<Mutex<SlotState>> = Lazy::new(|| Mutex::new(SlotState { total: 0, by_account: HashMap::new() }));
static GLOBAL_CAP_SET: OnceCell<usize> = OnceCell::new();
static GLOBAL_CAP: Lazy<usize> = Lazy::new(|| {
    GLOBAL_CAP_SET.get().copied().or_else(|| std::env::var("INTEGRATION_SCRIPT_SLOTS").ok().and_then(|v| v.parse().ok())).filter(|n| *n > 0).unwrap_or(8)
});

/// Set the global cap before the first script runs (tests run many accounts at once). Only the
/// first call counts; `INTEGRATION_SCRIPT_SLOTS` sets it in production.
pub fn set_global_cap(n: usize) {
    let _ = GLOBAL_CAP_SET.set(n);
}

/// A held slot; dropping it frees the slot.
pub struct Slot {
    account: Uuid,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut s = SLOT_STATE.lock().unwrap();
        s.total = s.total.saturating_sub(1);
        if let Some(n) = s.by_account.get_mut(&self.account) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                s.by_account.remove(&self.account);
            }
        }
    }
}

impl Slots {
    /// Take a slot for `account` now, or None when the account or the server is at its cap.
    pub fn try_take(account: Uuid) -> Option<Slot> {
        let mut s = SLOT_STATE.lock().unwrap();
        let mine = s.by_account.get(&account).copied().unwrap_or(0);
        if s.total >= *GLOBAL_CAP || mine >= PER_ACCOUNT {
            return None;
        }
        s.total += 1;
        *s.by_account.entry(account).or_insert(0) += 1;
        Some(Slot { account })
    }

    /// Wait up to `wait` for a slot (for requests a person is waiting on, like a test run).
    pub async fn take_within(account: Uuid, wait: Duration) -> Option<Slot> {
        let until = Instant::now() + wait;
        loop {
            if let Some(s) = Self::try_take(account) {
                return Some(s);
            }
            if Instant::now() >= until {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

// ---------- Parent: running the child ----------

static PROGRAM: OnceCell<PathBuf> = OnceCell::new();

/// Use another binary for sandboxed runs (tests point this at the built `dumbmail` binary; the
/// server uses itself). Only the first call counts.
pub fn set_program(path: impl Into<PathBuf>) {
    let _ = PROGRAM.set(path.into());
}

fn program() -> Result<PathBuf, String> {
    if let Some(p) = PROGRAM.get() {
        return Ok(p.clone());
    }
    std::env::current_exe().map_err(|e| format!("couldn't find the script runner: {e}"))
}

/// How a run ended, for callers (and tests) that want more than the result.
#[derive(Debug)]
pub struct Report {
    pub result: Result<Value, String>,
    /// The child's process id (already reaped when this is returned).
    pub pid: Option<u32>,
    pub elapsed: Duration,
}

/// Run `script`'s `handle(input)` in a sandboxed process and return what it returned, as JSON.
/// The caller must hold a [`Slot`].
pub async fn run(_slot: &Slot, script: &str, input: &Value) -> Result<Value, String> {
    let program = program()?;
    run_program(&program, &[SUBCOMMAND], script, input, TIME_LIMIT).await.result
}

/// The same with an explicit program, arguments and time limit (tests use these to check the
/// parent's own limits against misbehaving children).
pub async fn run_program(program: &std::path::Path, args: &[&str], script: &str, input: &Value, time_limit: Duration) -> Report {
    let started = Instant::now();
    let fail = |msg: String| Report { result: Err(msg), pid: None, elapsed: started.elapsed() };
    if script.trim().is_empty() {
        return fail("the integration has no script yet".into());
    }
    if script.len() > SCRIPT_MAX_BYTES {
        return fail(format!("the script is {} KB; the limit is {} KB", script.len() / 1024, SCRIPT_MAX_BYTES / 1024));
    }
    let request = json!({"script": script, "input": input, "time_ms": time_limit.as_millis() as u64}).to_string();
    if request.len() > INPUT_MAX_BYTES + SCRIPT_MAX_BYTES {
        return fail(format!("the script's input is over {} MB", INPUT_MAX_BYTES / (1024 * 1024)));
    }
    let cpu_secs = time_limit.as_secs() + 2;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    // SAFETY: only async-signal-safe calls (setrlimit, prctl) between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let lim = |soft: u64, hard: u64| libc::rlimit { rlim_cur: soft as libc::rlim_t, rlim_max: hard as libc::rlim_t };
            let check = |rc: libc::c_int| if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) };
            // Close every descriptor the server had open above stdin, stdout and stderr (database
            // sockets, listeners, files another thread opened without close-on-exec), so the
            // script's process holds nothing but its three pipes.
            close_inherited_fds()?;
            check(libc::setrlimit(libc::RLIMIT_AS, &lim(CHILD_ADDRESS_SPACE, CHILD_ADDRESS_SPACE)))?;
            check(libc::setrlimit(libc::RLIMIT_CPU, &lim(cpu_secs, cpu_secs + 1)))?;
            check(libc::setrlimit(libc::RLIMIT_CORE, &lim(0, 0)))?;
            check(libc::setrlimit(libc::RLIMIT_FSIZE, &lim(0, 0)))?;
            check(libc::setrlimit(libc::RLIMIT_NOFILE, &lim(16, 16)))?;
            // Die with the server, and never gain privileges.
            check(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0))?;
            check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("couldn't start the script runner: {e}")),
    };
    let pid = child.id();
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let deadline = time_limit + KILL_GRACE;
    let io = async {
        let write = async {
            if let Some(mut w) = stdin.take() {
                // A child that exits early closes the pipe; that's reported by what it printed.
                let _ = w.write_all(request.as_bytes()).await;
                let _ = w.shutdown().await;
            }
        };
        let read = async {
            let mut out = Vec::new();
            let Some(r) = stdout.as_mut() else { return Ok(out) };
            let mut buf = [0u8; 16 * 1024];
            loop {
                let n = r.read(&mut buf).await.map_err(|e| format!("couldn't read the script's result: {e}"))?;
                if n == 0 {
                    return Ok(out);
                }
                // Cap while reading: never buffer more than the limit (plus the envelope).
                if out.len() + n > OUTPUT_MAX_BYTES + 64 {
                    return Err(format!("the script returned more than {} KB; the limit is {} KB", OUTPUT_MAX_BYTES / 1024, OUTPUT_MAX_BYTES / 1024));
                }
                out.extend_from_slice(&buf[..n]);
            }
        };
        let ((), read) = tokio::join!(write, read);
        read
    };
    let outcome = tokio::time::timeout(deadline, io).await;
    let result = match outcome {
        Err(_) => {
            kill_group(pid);
            Err(format!("the script ran longer than {} seconds and was stopped", time_limit.as_secs_f64()))
        }
        Ok(Err(msg)) => {
            kill_group(pid);
            Err(msg)
        }
        Ok(Ok(out)) => {
            // Wait for the exit (bounded: the child has closed stdout, so it's finishing).
            match tokio::time::timeout(KILL_GRACE, child.wait()).await {
                Ok(Ok(status)) if status.success() => parse_reply(&out),
                Ok(Ok(status)) => Err(died(status)),
                _ => {
                    kill_group(pid);
                    Err("the script runner didn't finish and was stopped".into())
                }
            }
        }
    };
    // Always reap, so no process is left behind.
    let _ = child.start_kill();
    let _ = child.wait().await;
    Report { result, pid, elapsed: started.elapsed() }
}

/// Close every file descriptor above 2. Called between fork and exec, so only async-signal-safe
/// calls: `close_range(2)` where the kernel has it (Linux 5.9+), otherwise `close(2)` on each
/// descriptor up to the current limit. Fails (and the child never starts) if any stays open.
fn close_inherited_fds() -> std::io::Result<()> {
    // SAFETY: plain system calls on this (forked, single-threaded) process's own descriptors.
    unsafe {
        if libc::syscall(libc::SYS_close_range, 3 as libc::c_uint, libc::c_uint::MAX, 0 as libc::c_uint) == 0 {
            return Ok(());
        }
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let top = if lim.rlim_cur == libc::RLIM_INFINITY { 65_536 } else { lim.rlim_cur.min(1 << 20) as libc::c_int };
        for fd in 3..top {
            if libc::close(fd) != 0 && *libc::__errno_location() != libc::EBADF {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: plain kill(2) on the child's own process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

fn died(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(libc::SIGXCPU) | Some(libc::SIGKILL) => "the script used too much CPU time and was stopped".into(),
        Some(sig) => format!("the script stopped unexpectedly (signal {sig}); it may have used too much memory"),
        None => "the script runner failed (it may have used too much memory)".into(),
    }
}

fn parse_reply(out: &[u8]) -> Result<Value, String> {
    let v: Value = serde_json::from_slice(out).map_err(|_| "the script runner gave no usable answer".to_string())?;
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        return Err(e.to_string());
    }
    v.get("ok").cloned().ok_or_else(|| "the script runner gave no usable answer".into())
}

// ---------- Child: the hidden subcommand ----------

/// Entry point for `dumbmail sandbox-run`: read `{script, input, time_ms}` on stdin, run, write
/// `{"ok": result}` or `{"error": message}` on stdout. Returns the exit code.
pub fn child_main() -> i32 {
    let mut raw = Vec::new();
    let limit = (INPUT_MAX_BYTES + SCRIPT_MAX_BYTES + 1024) as u64;
    if std::io::stdin().take(limit + 1).read_to_end(&mut raw).is_err() || raw.len() as u64 > limit {
        return reply_error("the script's input is too large");
    }
    let req: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return reply_error("the script runner got no usable request"),
    };
    drop(raw);
    let script = req["script"].as_str().unwrap_or("");
    let input = req["input"].to_string();
    let ms = req["time_ms"].as_u64().unwrap_or(TIME_LIMIT.as_millis() as u64).min(10_000);
    match run_engine(script, &input, Duration::from_millis(ms)) {
        Ok(text) => {
            let mut out = std::io::stdout().lock();
            let ok = out.write_all(b"{\"ok\":").and_then(|_| out.write_all(text.as_bytes())).and_then(|_| out.write_all(b"}")).and_then(|_| out.flush());
            if ok.is_err() { 1 } else { 0 }
        }
        Err(e) => reply_error(&e),
    }
}

fn reply_error(msg: &str) -> i32 {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(json!({"error": msg}).to_string().as_bytes());
    let _ = out.flush();
    0
}

/// Run the script in QuickJS on this thread and return the JSON text of its result. Used by the
/// child process only.
pub fn run_engine(script: &str, input_json: &str, time_limit: Duration) -> Result<String, String> {
    if script.trim().is_empty() {
        return Err("the integration has no script yet".into());
    }
    if script.len() > SCRIPT_MAX_BYTES {
        return Err(format!("the script is {} KB; the limit is {} KB", script.len() / 1024, SCRIPT_MAX_BYTES / 1024));
    }
    let rt = Runtime::new().map_err(|e| format!("couldn't start the script engine: {e}"))?;
    rt.set_memory_limit(MEMORY_LIMIT_BYTES);
    rt.set_max_stack_size(STACK_LIMIT_BYTES);
    rt.set_gc_threshold(8 * 1024 * 1024);
    let deadline = Instant::now() + time_limit;
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = timed_out.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        if Instant::now() >= deadline {
            flag.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    })));
    let ctx = Context::full(&rt).map_err(|e| format!("couldn't start the script engine: {e}"))?;
    let secs = time_limit.as_secs_f64();
    let fail = |ctx: &rquickjs::Ctx<'_>, e: rquickjs::Error, during: &str| -> String {
        if timed_out.load(Ordering::Relaxed) {
            return format!("the script ran longer than {secs} seconds and was stopped");
        }
        let detail = match e {
            rquickjs::Error::Exception => {
                let thrown = ctx.catch();
                match thrown.as_exception() {
                    Some(ex) => {
                        let msg = ex.message().unwrap_or_default();
                        let at = ex.stack().unwrap_or_default().lines().find(|l| l.contains("<input>") || l.contains("script")).unwrap_or("").trim().to_string();
                        if at.is_empty() { msg } else { format!("{msg} ({at})") }
                    }
                    None => ctx.json_stringify(thrown).ok().flatten().and_then(|s| s.to_string().ok()).unwrap_or_else(|| "a non-error value was thrown".into()),
                }
            }
            rquickjs::Error::Allocation => "out of memory".into(),
            other => other.to_string(),
        };
        let detail = if detail.contains("out of memory") {
            format!("the script used more than {} MB of memory and was stopped", MEMORY_LIMIT_BYTES / (1024 * 1024))
        } else if detail.contains("stack overflow") {
            "the script recursed too deeply (stack overflow)".to_string()
        } else {
            detail
        };
        format!("{during}: {}", truncate(&detail, 1000))
    };
    ctx.with(|ctx| {
        let opts = {
            let mut o = rquickjs::context::EvalOptions::default();
            o.strict = false;
            o.global = true;
            o.filename = Some("script".into());
            o
        };
        ctx.eval_with_options::<(), _>(script.as_bytes().to_vec(), opts).map_err(|e| fail(&ctx, e, "the script failed to load"))?;
        let handle: Function = ctx
            .globals()
            .get::<_, Option<Function>>("handle")
            .map_err(|e| fail(&ctx, e, "the script failed to load"))?
            .ok_or_else(|| "the script must define `function handle(input) { ... }`".to_string())?;
        let input = ctx.json_parse(input_json.as_bytes().to_vec()).map_err(|e| fail(&ctx, e, "couldn't pass the input in"))?;
        let result: rquickjs::Value = handle.call((input,)).map_err(|e| fail(&ctx, e, "the script threw an error"))?;
        if result.is_promise() {
            return Err("handle() returned a Promise; it must return a list of actions directly (no async)".into());
        }
        let text = ctx.json_stringify(result).map_err(|e| fail(&ctx, e, "couldn't read the script's result"))?;
        let Some(text) = text else { return Ok("null".to_string()) };
        let text = text.to_string().map_err(|e| fail(&ctx, e, "couldn't read the script's result"))?;
        if text.len() > OUTPUT_MAX_BYTES {
            return Err(format!("the script returned {} KB; the limit is {} KB", text.len() / 1024, OUTPUT_MAX_BYTES / 1024));
        }
        Ok(text)
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    //! The engine itself, in-process. The process sandbox (kills, caps, leftovers) is tested
    //! end to end in `tests/integrations.rs` against the real `dumbmail sandbox-run`.
    use super::*;

    fn go(script: &str, input: Value) -> Result<Value, String> {
        run_engine(script, &input.to_string(), Duration::from_millis(500)).map(|t| serde_json::from_str(&t).unwrap())
    }

    #[test]
    fn returns_plain_data() {
        let v = go("function handle(i) { return [{type: 'log', message: 'hi ' + i.payload.name}] }", json!({"payload": {"name": "Ann"}})).unwrap();
        assert_eq!(v, json!([{"type": "log", "message": "hi Ann"}]));
    }

    #[test]
    fn needs_a_handle_function() {
        assert!(go("var x = 1;", json!({})).unwrap_err().contains("function handle"));
        assert!(go("function handle( {", json!({})).unwrap_err().contains("failed to load"));
        assert!(go("", json!({})).is_err());
    }

    #[test]
    fn reports_thrown_errors() {
        let e = go("function handle(i) { throw new Error('bad payload') }", json!({})).unwrap_err();
        assert!(e.contains("bad payload"), "{e}");
    }

    #[test]
    fn has_no_host_access() {
        for probe in ["require", "process", "fetch", "XMLHttpRequest", "setTimeout", "setInterval", "std", "os", "Deno", "Bun", "print", "console"] {
            let v = go(&format!("function handle(i) {{ return typeof globalThis['{probe}'] }}"), json!({})).unwrap();
            assert_eq!(v, json!("undefined"), "{probe}");
        }
        assert!(go("function handle(i) { return import('fs') }", json!({})).unwrap_err().contains("Promise"));
    }

    #[test]
    fn functions_and_undefined_come_out_as_json() {
        assert_eq!(go("function handle(i) { return undefined }", json!({})).unwrap(), Value::Null);
        assert_eq!(go("function handle(i) { return [function () {}, NaN] }", json!({})).unwrap(), json!([null, null]));
    }
}
