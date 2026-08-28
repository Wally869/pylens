//! Dynamic execution of untrusted Python inside an **nsjail** sandbox.
//!
//! Every execution is jailed — there is deliberately **no unsandboxed launcher**. The sandbox
//! is always `nsjail` on a Linux kernel; the only per-host difference is how that kernel is
//! reached:
//!
//! * **Linux**:   `bash -lc 'exec nsjail … -- python3 worker.py'`           (native)
//! * **Windows**: `wsl -d <distro> -- bash -lc 'exec nsjail … -- python3 worker.py'`
//!   — WSL2 is itself a Linux kernel *and* a lightweight VM, so untrusted code sits behind
//!   `nsjail → WSL2 VM → host` without any container.
//!
//! The nsjail policy and `worker.py` are deployed by `scripts/provision-sandbox.sh` into
//! `$HOME/.local/share/pylens/`; the launcher references them via `$HOME` evaluated by the
//! in-distro shell, so this side never needs to resolve the Linux home path.
//!
//! Two launchers implement [`Sandbox`]:
//! * [`Nsjail`] — one fresh jailed process per call (maximum isolation).
//! * [`NsjailPool`] — a pool of long-lived `--serve` fork-server jails (amortizes interpreter
//!   startup; each request still runs in a forked child, so untrusted state never leaks
//!   between calls). See docs/DESIGN.md "Execution layer".

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Condvar, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Serialize)]
struct Request<'a> {
    source: &'a str,
    /// The function/method to call; `None` makes this a load-only probe (exec the module,
    /// optionally build the receiver, report whether it loaded — no call).
    #[serde(rename = "fn")]
    fn_name: Option<&'a str>,
    args: &'a [Value],
    /// Keyword-only arguments, name → value.
    #[serde(skip_serializing_if = "Map::is_empty")]
    kwargs: Map<String, Value>,
    /// For a method call: the class to instantiate as the receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<&'a str>,
    /// Constructor arguments for `class`.
    #[serde(skip_serializing_if = "Option::is_none")]
    ctor_args: Option<&'a [Value]>,
}

/// A raised exception observed at runtime.
#[derive(Debug, Clone, Deserialize)]
pub struct Exc {
    #[serde(rename = "type")]
    pub ty: String,
    pub message: String,
}

/// A structured harness/setup failure — *not* a Python `raise` from the function under test.
/// `stage` is where it happened (`setup`, `ctor`, `harness`, `resource`, `serialize`,
/// `bad_request`); `kind` is the exception type (e.g. `ModuleNotFoundError`) or a harness code;
/// `module` carries the missing module for import failures.
///
/// `stage == "resource"` is a **resource kill**: the sandbox ran the function out of memory
/// (`kind = "MemoryError"`), recursion depth (`kind = "RecursionError"`), or wall time
/// (`kind = "timeout"`). This is strictly distinct from a semantic Python raise — see
/// [`HarnessError::is_resource`] and `Case::outcome` in `record.rs`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HarnessError {
    pub stage: String,
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
}

impl HarnessError {
    /// Whether this error is a resource kill (out-of-memory, recursion limit, timeout) rather
    /// than a harness/setup failure — an artifact of the sandbox, never the function's behavior.
    pub fn is_resource(&self) -> bool {
        self.stage == "resource"
    }
}

/// The observed effects of one execution.
#[derive(Debug, Clone, Deserialize)]
pub struct CallResult {
    pub ok: bool,
    #[serde(rename = "return")]
    pub ret: Value,
    /// Argument state captured (tagged) *before* the call — the baseline for mutation diffs.
    #[serde(default)]
    pub args_pre: Option<Vec<Value>>,
    pub args_post: Option<Vec<Value>>,
    /// Keyword-only argument state (name → serialized value) captured *before* the call — the
    /// baseline for kwarg mutation diffs.
    #[serde(default)]
    pub kwargs_pre: Option<Map<String, Value>>,
    #[serde(default)]
    pub kwargs_post: Option<Map<String, Value>>,
    pub exception: Option<Exc>,
    pub return_aliases_arg: Option<i64>,
    /// Anything the function wrote to stdout during the call (captured, not leaked).
    #[serde(default)]
    pub stdout: Option<String>,
    /// Anything the function wrote to stderr during the call (captured separately from stdout).
    #[serde(default)]
    pub stderr: Option<String>,
    /// Receiver (`self`) attribute state before the method call — methods only.
    #[serde(default)]
    pub self_pre: Option<Value>,
    /// Receiver (`self`) attribute state after the method call — methods only.
    #[serde(default)]
    pub self_post: Option<Value>,
    /// Harness-level failure (function missing, bad source, import/ctor failure, timeout, …) —
    /// not a Python raise from the function under test.
    pub error: Option<HarnessError>,
    /// 1-based line numbers reached in the module under test during the call (and, for a
    /// generator, its draining) — sorted ascending. Populated even when the call ends in `error`
    /// (a resource kill still ran some lines first). See `record::coverage`.
    #[serde(default)]
    pub lines: Vec<u32>,
    /// `(prev_line, cur_line)` transitions traced within the module under test during the call —
    /// sorted ascending, per-frame (a call into another traced function never contributes a
    /// spurious arc from the caller's line to the callee's). See `record::branch_report_for`,
    /// which turns these into per-branch-outcome coverage.
    #[serde(default)]
    pub arcs: Vec<(u32, u32)>,
}

/// A jailed Python execution → observed effects. Implementors provide [`Sandbox::transport`]
/// (send one encoded request, get the result); `call` / `call_method` are built on top.
pub trait Sandbox {
    /// Send one already-encoded request to a jailed worker and parse its response.
    fn transport(&self, body: &[u8]) -> Result<CallResult, String>;

    /// Execute free function `fn_name(*args, **kwargs)`.
    fn call(
        &self,
        source: &str,
        fn_name: &str,
        args: &[Value],
        kwargs: &[(String, Value)],
    ) -> Result<CallResult, String> {
        let body = encode_request(source, Some(fn_name), args, kwargs, None, None)?;
        self.transport(&body)
    }

    /// Execute `class(*ctor_args).method(*args, **kwargs)`, capturing receiver pre/post state.
    fn call_method(
        &self,
        source: &str,
        class: &str,
        ctor_args: &[Value],
        method: &str,
        args: &[Value],
        kwargs: &[(String, Value)],
    ) -> Result<CallResult, String> {
        let body = encode_request(source, Some(method), args, kwargs, Some(class), Some(ctor_args))?;
        self.transport(&body)
    }

    /// Load-only probe: exec `source` (and, if `class` is given, build the receiver with
    /// `ctor_args`) without calling anything. `ok` means it loaded; otherwise `error` says why
    /// (e.g. a `ModuleNotFoundError` at module scope, or a constructor that raised). Lets the
    /// caller learn a module won't import once, instead of N identical per-case failures.
    fn probe_load(
        &self,
        source: &str,
        class: Option<&str>,
        ctor_args: Option<&[Value]>,
    ) -> Result<CallResult, String> {
        let body = encode_request(source, None, &[], &[], class, ctor_args)?;
        self.transport(&body)
    }
}

fn encode_request(
    source: &str,
    fn_name: Option<&str>,
    args: &[Value],
    kwargs: &[(String, Value)],
    class: Option<&str>,
    ctor_args: Option<&[Value]>,
) -> Result<Vec<u8>, String> {
    let req = Request {
        source,
        fn_name,
        args,
        kwargs: kwargs.iter().cloned().collect(),
        class,
        ctor_args,
    };
    serde_json::to_vec(&req).map_err(|e| e.to_string())
}

fn parse_response(bytes: &[u8]) -> Result<CallResult, String> {
    serde_json::from_slice(bytes).map_err(|e| {
        format!(
            "bad worker output: {e}\n{}",
            String::from_utf8_lossy(bytes)
        )
    })
}

/// The in-distro shell script that launches the jailed worker. `$HOME` is expanded by the
/// in-distro shell so this side never resolves the Linux home path. In `serve` mode the jail's
/// wall `time_limit` is disabled (`--time_limit 0`) because the process is long-lived; the
/// fork-server bounds each request itself.
fn jail_script(serve: bool) -> String {
    let (serve_flag, extra) = if serve {
        (" --serve", " --time_limit 0")
    } else {
        ("", "")
    };
    format!(
        "exec nsjail --config \"$HOME/.local/share/pylens/pylens.nsjail.cfg\"{extra} \
         --bindmount_ro \"$HOME/.local/share/pylens:/pylens\" \
         -- /usr/bin/python3 /pylens/worker.py{serve_flag}"
    )
}

/// Returns the distro to target on Windows (env `PYLENS_WSL_DISTRO`, default `Ubuntu`).
fn wsl_distro() -> String {
    std::env::var("PYLENS_WSL_DISTRO").unwrap_or_else(|_| "Ubuntu".to_string())
}

/// Build the host command that runs `script` on a Linux kernel: directly on Linux, or via
/// `wsl` on Windows.
fn host_command(script: &str) -> Command {
    if cfg!(target_os = "windows") {
        let mut c = Command::new("wsl");
        c.args(["-d", &wsl_distro(), "--", "bash", "-lc", script]);
        c
    } else {
        let mut c = Command::new("bash");
        c.args(["-lc", script]);
        c
    }
}

fn launcher_desc() -> String {
    if cfg!(target_os = "windows") {
        format!("wsl -d {} -- nsjail", wsl_distro())
    } else {
        "nsjail".to_string()
    }
}

/// Probe whether the sandbox is provisioned and reachable (nsjail on PATH + the worker and
/// policy deployed). Cheap; lets callers/tests skip gracefully rather than fail when the
/// sandbox isn't set up. There is no unsandboxed fallback — if this fails, execution can't run.
pub fn probe() -> Result<(), String> {
    let script = "command -v nsjail >/dev/null 2>&1 && \
                  test -f \"$HOME/.local/share/pylens/worker.py\" && \
                  test -f \"$HOME/.local/share/pylens/pylens.nsjail.cfg\"";
    let status = host_command(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("cannot reach sandbox host ({}): {e}", launcher_desc()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "sandbox not provisioned ({}); run scripts/provision-sandbox.sh",
            launcher_desc()
        ))
    }
}

/// One fresh jailed CPython process per call. Maximum isolation, higher per-call cost.
pub struct Nsjail;

impl Nsjail {
    pub fn new() -> Self {
        Nsjail
    }
}

impl Default for Nsjail {
    fn default() -> Self {
        Nsjail
    }
}

impl Sandbox for Nsjail {
    fn transport(&self, body: &[u8]) -> Result<CallResult, String> {
        let mut child = host_command(&jail_script(false))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!(
                    "spawn sandbox ({}): {e}. Is the sandbox provisioned? \
                     run scripts/provision-sandbox.sh",
                    launcher_desc()
                )
            })?;

        child
            .stdin
            .take()
            .ok_or("no stdin")?
            .write_all(body)
            .map_err(|e| e.to_string())?;

        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if out.stdout.is_empty() {
            return Err(format!(
                "sandbox produced no output (jail not provisioned or policy rejected the run). \
                 stderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        parse_response(&out.stdout)
    }
}

/// A long-lived jailed `--serve` fork-server.
struct ServeWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl ServeWorker {
    fn spawn() -> Result<Self, String> {
        let mut child = host_command(&jail_script(true))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                format!(
                    "spawn sandbox pool ({}): {e}. Is the sandbox provisioned? \
                     run scripts/provision-sandbox.sh",
                    launcher_desc()
                )
            })?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        Ok(ServeWorker {
            child,
            stdin,
            stdout,
        })
    }

    /// Send one request, read one newline-delimited response. Errors mean the worker is dead.
    fn exchange(&mut self, body: &[u8]) -> Result<CallResult, String> {
        self.stdin.write_all(body).map_err(|e| e.to_string())?;
        self.stdin.write_all(b"\n").map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())?;

        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("serve worker closed its output".to_string());
        }
        parse_response(line.trim_end().as_bytes())
    }
}

impl Drop for ServeWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A pool of long-lived jailed fork-server workers. Amortizes interpreter + import startup
/// across the many calls of dataset building while keeping per-request isolation (each request
/// runs in a forked child inside the jail).
pub struct NsjailPool {
    idle: Mutex<Vec<ServeWorker>>,
    available: Condvar,
    size: usize,
}

impl NsjailPool {
    /// Create a pool of `size` jailed workers (eagerly spawned).
    pub fn new(size: usize) -> Result<Self, String> {
        assert!(size > 0, "pool size must be > 0");
        let mut idle = Vec::with_capacity(size);
        for _ in 0..size {
            idle.push(ServeWorker::spawn()?);
        }
        Ok(NsjailPool {
            idle: Mutex::new(idle),
            available: Condvar::new(),
            size,
        })
    }

    fn acquire(&self) -> ServeWorker {
        let mut idle = self.idle.lock().unwrap();
        loop {
            if let Some(w) = idle.pop() {
                return w;
            }
            idle = self.available.wait(idle).unwrap();
        }
    }

    fn release(&self, w: ServeWorker) {
        let mut idle = self.idle.lock().unwrap();
        idle.push(w);
        drop(idle);
        self.available.notify_one();
    }
}

impl Sandbox for NsjailPool {
    fn transport(&self, body: &[u8]) -> Result<CallResult, String> {
        let mut worker = self.acquire();
        match worker.exchange(body) {
            Ok(result) => {
                self.release(worker);
                Ok(result)
            }
            Err(e) => {
                // Worker died mid-exchange; drop it and bring the pool back to full strength
                // so the pool stays usable, then surface the error for this call.
                drop(worker);
                match ServeWorker::spawn() {
                    Ok(fresh) => self.release(fresh),
                    Err(spawn_err) => {
                        // Couldn't replace it; let waiters proceed rather than starve.
                        let _ = spawn_err;
                        self.available.notify_one();
                    }
                }
                Err(format!("serve worker failed: {e}"))
            }
        }
    }
}

impl NsjailPool {
    /// The configured pool size.
    pub fn size(&self) -> usize {
        self.size
    }
}
