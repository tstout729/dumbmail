//! Runs an integration's translation script in QuickJS with hard limits.
//!
//! Every run gets a fresh runtime with no host functions at all (no network, files, timers or
//! module loader), a memory cap, a stack cap and a wall-clock deadline enforced by QuickJS's
//! interrupt handler, so a runaway or hostile script stops instead of hanging a worker. Input goes
//! in and output comes out as JSON text through the engine's own JSON functions, so nothing the
//! script defines can reach Rust except plain data. Runs happen on blocking threads, a few at a time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use rquickjs::{Context, Function, Runtime};
use serde_json::Value;
use tokio::sync::Semaphore;

/// Largest script accepted.
pub const SCRIPT_MAX_BYTES: usize = 256 * 1024;
/// Largest result (the JSON text of the returned actions).
pub const OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// Memory one run may allocate, input included.
pub const MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// JavaScript stack one run may use.
pub const STACK_LIMIT_BYTES: usize = 512 * 1024;
/// Wall-clock time one run may take.
pub const TIME_LIMIT: Duration = Duration::from_secs(2);

/// Scripts running at once across the server; the rest wait their turn.
static SLOTS: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(4));

/// Run `script`'s `handle(input)` and return what it returned, as JSON.
pub async fn run(script: String, input: Value) -> Result<Value, String> {
    let input = serde_json::to_string(&input).map_err(|e| format!("couldn't prepare the input: {e}"))?;
    let _slot = SLOTS.acquire().await.map_err(|_| "the script runner is shutting down".to_string())?;
    tokio::task::spawn_blocking(move || run_blocking(&script, &input, TIME_LIMIT))
        .await
        .map_err(|_| "the script runner stopped unexpectedly".to_string())?
}

/// The same, on the current thread, with a custom time limit (tests use a short one).
pub fn run_blocking(script: &str, input_json: &str, time_limit: Duration) -> Result<Value, String> {
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
    let out: Result<String, String> = ctx.with(|ctx| {
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
    });
    let text = out?;
    serde_json::from_str(&text).map_err(|e| format!("the script's result isn't valid JSON: {e}"))
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
    use super::*;
    use serde_json::json;

    fn go(script: &str, input: Value) -> Result<Value, String> {
        run_blocking(script, &input.to_string(), Duration::from_millis(500))
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
        let e = go("function handle(i) { return i.nope.deeper }", json!({})).unwrap_err();
        assert!(e.contains("threw"), "{e}");
    }

    #[test]
    fn stops_infinite_loops() {
        let t = Instant::now();
        let e = go("function handle(i) { while (true) {} }", json!({})).unwrap_err();
        assert!(e.contains("longer than"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(3));
        // A loop at load time is stopped too.
        let e = go("for (;;) {} function handle(i) { return [] }", json!({})).unwrap_err();
        assert!(e.contains("longer than"), "{e}");
        // Catching the interrupt doesn't help.
        let e = go("function handle(i) { for (;;) { try { while (true) {} } catch (e) {} } }", json!({})).unwrap_err();
        assert!(e.contains("longer than"), "{e}");
    }

    #[test]
    fn stops_regex_backtracking() {
        let t = Instant::now();
        let e = go("function handle(i) { return [/^(a+)+$/.test('a'.repeat(40) + 'b')] }", json!({})).unwrap_err();
        assert!(e.contains("longer than"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn stops_memory_bombs() {
        let e = go("function handle(i) { var a = []; while (true) { a.push('x'.repeat(1 << 20) + a.length) } }", json!({})).unwrap_err();
        assert!(e.contains("memory"), "{e}");
        let e = go("function handle(i) { var s = 'x'; while (true) { s = s + s } }", json!({})).unwrap_err();
        assert!(e.contains("memory") || e.contains("too long"), "{e}");
    }

    #[test]
    fn stops_deep_recursion() {
        let e = go("function f(n) { return f(n + 1) + 1 } function handle(i) { return f(0) }", json!({})).unwrap_err();
        assert!(e.contains("stack") || e.contains("recursed"), "{e}");
    }

    #[test]
    fn caps_output() {
        let e = go("function handle(i) { var a = []; for (var k = 0; k < 3000; k++) a.push({type: 'log', message: 'y'.repeat(500)}); return a }", json!({})).unwrap_err();
        assert!(e.contains("limit"), "{e}");
    }

    #[test]
    fn has_no_host_access() {
        for probe in ["require", "process", "fetch", "XMLHttpRequest", "setTimeout", "setInterval", "std", "os", "Deno", "Bun", "import"] {
            let v = go(&format!("function handle(i) {{ return typeof globalThis['{probe}'] }}"), json!({})).unwrap();
            assert_eq!(v, json!("undefined"), "{probe}");
        }
        // Dynamic import has no loader to reach.
        let v = go("function handle(i) { var p = import('fs'); return typeof p }", json!({}));
        assert!(matches!(v, Ok(Value::String(_)) | Err(_)));
        assert!(go("function handle(i) { return import('fs') }", json!({})).unwrap_err().contains("Promise"));
    }

    #[test]
    fn prototype_tricks_stay_inside() {
        // Poisoning JSON or prototypes only changes what the script itself returns.
        let s = "JSON.stringify = function () { return '{\"type\":\"evil\"}' };
                 Object.prototype.toJSON = undefined;
                 Array.prototype.push = function () { throw new Error('nope') };
                 function handle(i) { return [{type: 'log', message: 'ok'}] }";
        assert_eq!(go(s, json!({})).unwrap(), json!([{"type": "log", "message": "ok"}]));
        // A toJSON getter runs inside the sandbox, under the same limits.
        let s = "function handle(i) { return {toJSON: function () { while (true) {} }} }";
        assert!(go(s, json!({})).unwrap_err().contains("longer than"));
        // Polluting the input's prototype doesn't affect the next run (each run is a fresh engine).
        let s = "function handle(i) { Object.prototype.polluted = 1; return [] }";
        go(s, json!({})).unwrap();
        assert_eq!(go("function handle(i) { return typeof ({}).polluted }", json!({})).unwrap(), json!("undefined"));
        // __proto__ in the input is just data.
        let v = go("function handle(i) { return [typeof ({}).isAdmin, i.payload.__proto__ ? 1 : 0] }", serde_json::from_str(r#"{"payload": {"__proto__": {"isAdmin": true}}}"#).unwrap()).unwrap();
        assert_eq!(v[0], json!("undefined"));
    }

    #[test]
    fn rejects_huge_scripts() {
        let big = format!("function handle(i) {{ return [] }} //{}", "x".repeat(SCRIPT_MAX_BYTES));
        assert!(go(&big, json!({})).unwrap_err().contains("limit"));
    }

    #[test]
    fn functions_and_undefined_come_out_as_json() {
        assert_eq!(go("function handle(i) { return undefined }", json!({})).unwrap(), Value::Null);
        assert_eq!(go("function handle(i) { return [function () {}, NaN] }", json!({})).unwrap(), json!([null, null]));
    }
}
