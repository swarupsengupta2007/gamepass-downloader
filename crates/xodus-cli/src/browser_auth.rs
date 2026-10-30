//! Windows sign-in through Microsoft Edge or Google Chrome.
//!
//! WebView2 does not offer passkey or authenticator sign-in. This host opens a
//! real browser with a disposable profile and drives it over Chrome's private
//! DevTools pipe. On Windows that pipe is a pair of inherited handles named by
//! `--remote-debugging-io-pipes`. It is not a listening port, and stdin/stdout
//! are not the pipe.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use serde_json::{Value, json};
use xodus::models::live::{DAProperty, HostBridgeMessage};

use super::{HandlerControl, HandlerResult, RuntimeAction, RuntimeCommands, SessionHandler, SessionId, WebviewRequest};

const TRUSTED: &[&str] = &[
    "login.live.com",
    "account.live.com",
    "login.microsoftonline.com",
    "login.microsoft.com",
    "account.microsoft.com",
    "sisu.xboxlive.com",
];
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

struct Browser {
    path: PathBuf,
    label: &'static str,
}

struct Host {
    cdp: Cdp,
    _profile: tempfile::TempDir,
    _process: BrowserProcess,
    active: Option<SessionId>,
    last_token: Option<String>,
    navigated: bool,
}

struct BrowserProcess(OwnedHandle);

struct Cdp {
    stdin: File,
    incoming: Receiver<Result<Value, String>>,
    next: u64,
    ready: HashMap<u64, Result<Value, String>>,
    ignore: HashSet<u64>,
    events: VecDeque<Value>,
    page_session: String,
    target_id: String,
    main_frame: String,
    document_url: Option<String>,
    document_headers: Vec<(String, String)>,
}

pub fn run<T: SessionHandler>(mut handler: T) -> HandlerResult<Option<T::Output>> {
    let _ = std::fs::write(trace_path(), "");
    let browser = find_browser().ok_or_else(|| {
        std::io::Error::other(
            "Sign-in needs Microsoft Edge or Google Chrome so the passkey and authenticator prompts can appear. Neither was found.",
        )
    })?;
    eprintln!(
        "Opening {} for sign-in. Approve the passkey or authenticator prompt in that window.",
        browser.label
    );
    let mut host = Host::launch(&browser).map_err(|error| -> Box<dyn std::error::Error> {
        if update_pending(&browser) {
            sign_in_log(&format!("{} relaunched itself to finish an update", browser.label));
            return std::io::Error::other(format!(
                "{0} is waiting to finish an update and restarted itself, which cuts off sign-in. Close every {0} window so the update can finish, then sign in again.",
                browser.label
            ))
            .into();
        }
        error
    })?;
    let mut next = 1u64;
    let mut commands = RuntimeCommands::new(next);
    handler.bootstrap(&mut commands)?;
    next = commands.next_session_id;
    host.apply(&mut commands)?;

    let mut result = None;
    while result.is_none() {
        host.drain(&mut handler, &mut next, &mut result)?;
        if result.is_some() {
            break;
        }
        let incoming = match host.cdp.incoming.recv() {
            Ok(Ok(message)) => Some(message),
            Ok(Err(error)) => {
                sign_in_log(&format!("browser closed ({error})"));
                host.close_or_fail(&mut handler, &mut next, &mut result, error)?;
                None
            }
            Err(_) => {
                sign_in_log("browser pipe closed");
                host.close_or_fail(
                    &mut handler,
                    &mut next,
                    &mut result,
                    "Sign-in window closed.".to_string(),
                )?;
                None
            }
        };
        if let Some(message) = incoming {
            host.cdp.ingest(message)?;
        }
    }
    Ok(result)
}

/// Whether the browser has an update installed but not yet swapped in (it
/// keeps the new launcher as `new_chrome.exe` / `new_msedge.exe` while a copy
/// is open). Started then, it relaunches itself on the new version and the
/// first process exits, and the relaunched one has no DevTools pipe to us.
fn update_pending(browser: &Browser) -> bool {
    let Some(name) = browser.path.file_name().and_then(|n| n.to_str()) else { return false };
    let application = browser.path.ancestors().find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("Application")));
    application.is_some_and(|dir| dir.join(format!("new_{name}")).is_file())
}

/// Google Chrome, then Microsoft Edge, but a browser with an update waiting
/// (see `update_pending`) only when there is no other.
fn find_browser() -> Option<Browser> {
    let mut browsers = installed_browsers();
    browsers.sort_by_key(update_pending);
    browsers.into_iter().next()
}

fn installed_browsers() -> Vec<Browser> {
    let mut found = Vec::new();
    let mut roots = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local));
    }
    if let Some(program) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(program));
    }
    if let Some(program) = std::env::var_os("ProgramFiles(x86)") {
        roots.push(PathBuf::from(program));
    }
    // A versioned binary is the browser itself. The copy directly under
    // Application is sometimes only a stub, so it is the fallback.
    for (folder, name, label) in [(r"Google\Chrome\Application", "chrome.exe", "Google Chrome"), (r"Microsoft\Edge\Application", "msedge.exe", "Microsoft Edge")] {
        if let Some(path) = roots.iter().find_map(|root| browser_binary(&root.join(folder), name)) {
            found.push(Browser { path, label });
        }
    }
    found
}

fn browser_binary(application: &Path, name: &str) -> Option<PathBuf> {
    newest_binary(application, name).or_else(|| {
        let direct = application.join(name);
        direct.is_file().then_some(direct)
    })
}

fn newest_binary(application: &std::path::Path, name: &str) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(application).ok()?.flatten() {
        let binary = entry.path().join(name);
        let Ok(metadata) = binary.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if best.as_ref().is_none_or(|(time, _)| modified > *time) {
            best = Some((modified, binary));
        }
    }
    best.map(|(_, path)| path)
}

impl Host {
    fn launch(browser: &Browser) -> HandlerResult<Self> {
        let profile = tempfile::TempDir::new()?;
        let (process, stdout, stdin) = spawn_browser(browser, profile.path())?;
        let (tx, incoming) = mpsc::channel();
        std::thread::spawn(move || read_pipe(stdout, tx));
        let cdp = Cdp {
            stdin,
            incoming,
            next: 0,
            ready: HashMap::new(),
            ignore: HashSet::new(),
            events: VecDeque::new(),
            page_session: String::new(),
            target_id: String::new(),
            main_frame: String::new(),
            document_url: None,
            document_headers: Vec::new(),
        };
        let mut host = Self {
            cdp,
            _profile: profile,
            _process: process,
            active: None,
            last_token: None,
            navigated: false,
        };
        host.cdp.attach()?;
        Ok(host)
    }

    fn apply(&mut self, commands: &mut RuntimeCommands) -> HandlerResult<()> {
        for action in commands.actions.drain(..) {
            match action {
                RuntimeAction::OpenSession { session_id, request } => self.open(session_id, request)?,
                RuntimeAction::CloseSession(session_id) => {
                    if self.active == Some(session_id) {
                        self.active = None;
                    }
                }
            }
        }
        Ok(())
    }

    fn open(&mut self, session_id: SessionId, request: WebviewRequest) -> HandlerResult<()> {
        let url = clean_page_url(&request.url);
        if !trusted_auth_url(&url) {
            sign_in_log(&format!("unsupported page {}", page_location(&url)));
            return Err(std::io::Error::other("Unsupported sign-in page.").into());
        }
        self.active = Some(session_id);
        self.last_token = None;
        self.navigated = true;
        self.cdp.document_url = Some(url.clone());
        self.cdp.document_headers = request
            .headers
            .iter()
            .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str().to_string(), value.to_string())))
            .collect();
        sign_in_log(&format!("open {}", page_location(&url)));
        if let Err(error) = self.cdp.navigate(&url) {
            sign_in_log(&format!("reopening the sign-in window after {error}"));
            self.cdp.navigate_in_new_target(&url)?;
        }
        Ok(())
    }

    fn drain<T: SessionHandler>(
        &mut self,
        handler: &mut T,
        next: &mut u64,
        result: &mut Option<T::Output>,
    ) -> HandlerResult<()> {
        while result.is_none() && let Some(event) = self.cdp.events.pop_front() {
            self.on_event(event, handler, next, result)?;
        }
        Ok(())
    }

    fn on_event<T: SessionHandler>(
        &mut self,
        event: Value,
        handler: &mut T,
        next: &mut u64,
        result: &mut Option<T::Output>,
    ) -> HandlerResult<()> {
        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
        let params = event.get("params").cloned().unwrap_or_else(|| json!({}));
        let session = event.get("sessionId").and_then(Value::as_str);
        if method != "Target.targetDestroyed" && session != Some(self.cdp.page_session.as_str()) {
            return Ok(());
        }
        match method {
            "Target.targetDestroyed" => {
                if params.get("targetId").and_then(Value::as_str) == Some(self.cdp.target_id.as_str()) {
                    sign_in_log("sign-in window closed");
                    self.finish_closed(handler, next, result)?;
                }
            }
            "Page.windowOpen" => {
                let url = params.get("url").and_then(Value::as_str).unwrap_or("");
                sign_in_log(&format!("popup {}", page_location(url)));
                return Err(std::io::Error::other("Sign-in window was blocked.").into());
            }
            "Page.frameNavigated" => {
                let frame = &params["frame"];
                if frame.get("parentId").is_some() {
                    return Ok(());
                }
                if let Some(id) = frame.get("id").and_then(Value::as_str) {
                    self.cdp.main_frame = id.to_string();
                }
                let url = frame.get("url").and_then(Value::as_str).unwrap_or("");
                sign_in_log(&format!("navigate {}", page_location(url)));
                if self.navigated && url != "about:blank" && !trusted_auth_url(url) {
                    return Err(std::io::Error::other("Sign-in left the Microsoft login page.").into());
                }
            }
            "Runtime.bindingCalled" => {
                let payload = params.get("payload").and_then(Value::as_str).unwrap_or("");
                let context = params.get("executionContextId").and_then(Value::as_i64);
                if payload.len() > 65_536 {
                    return Ok(());
                }
                if let Some(context) = context {
                    self.on_binding(payload, context, handler, next, result)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_binding<T: SessionHandler>(
        &mut self,
        payload: &str,
        context_id: i64,
        handler: &mut T,
        next: &mut u64,
        result: &mut Option<T::Output>,
    ) -> HandlerResult<()> {
        let Some(session) = self.active else {
            return Ok(());
        };
        let origin = match self.cdp.evaluate(context_id, "location.origin") {
            Ok(value) => value,
            Err(error) if context_is_gone(&error.to_string()) => {
                sign_in_log("page moved on");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let origin = origin
            .pointer("/result/value")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !trusted_origin(origin) || self.active != Some(session) {
            sign_in_log(&format!("ignored page message from {origin}"));
            return Ok(());
        }
        if log_diagnostic(payload) {
            return Ok(());
        }
        if payload.contains("\"sDAToken\"") {
            match serde_json::from_str::<DAProperty>(payload) {
                Ok(data) => {
                    if self.last_token.as_deref() == Some(data.da_token.as_str()) {
                        sign_in_log("duplicate account response");
                        return Ok(());
                    }
                    sign_in_log("account response received");
                    self.last_token = Some(data.da_token.clone());
                    *result = self.finish(handler, next, |handler, runtime| {
                        handler.on_token(session, data, runtime)
                    })?;
                    return Ok(());
                }
                Err(error) => {
                    let message = error.to_string();
                    if let Some(field) = message.strip_prefix("missing field ") {
                        sign_in_log(&format!("account response missing {field}"));
                    } else {
                        sign_in_log("account response had an unexpected shape");
                    }
                }
            }
        } else {
            sign_in_log(&format!("page message {}", payload_keys(payload)));
        }
        if let Ok(message) = serde_json::from_str::<HostBridgeMessage>(payload)
            && let Some(context) = message.get_context_invoke()
        {
            sign_in_log("host context requested");
            self.reply_context(context, context_id)?;
        }
        Ok(())
    }

    fn reply_context(&mut self, context: &str, context_id: i64) -> HandlerResult<()> {
        let message = json!({
            "type": "callback",
            "value": {
                "name": "CloudExperienceHost.getContext",
                "args": [
                    "CloudExperienceHost",
                    "TokenBroker",
                    "TokenBroker",
                    r#"{"PrivatePropertyBag":1,"PasswordlessConnect":1,"PreferAssociate":1,"ChromelessUI":0}"#
                ],
                "context": context
            }
        });
        let argument = serde_json::to_string(&message)?;
        let expression = format!(
            "globalThis[\"CloudExperienceHost.Bridge.dispatchMessage\"]({})",
            serde_json::to_string(&argument)?
        );
        match self.cdp.evaluate(context_id, &expression) {
            Ok(_) => Ok(()),
            Err(error) if context_is_gone(&error.to_string()) => {
                sign_in_log("page moved on before the host reply");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn close_or_fail<T: SessionHandler>(
        &mut self,
        handler: &mut T,
        next: &mut u64,
        result: &mut Option<T::Output>,
        error: String,
    ) -> HandlerResult<()> {
        if self.active.is_some() {
            self.finish_closed(handler, next, result)
        } else {
            Err(std::io::Error::other(error).into())
        }
    }

    fn finish_closed<T: SessionHandler>(
        &mut self,
        handler: &mut T,
        next: &mut u64,
        result: &mut Option<T::Output>,
    ) -> HandlerResult<()> {
        let Some(session) = self.active.take() else {
            return Err(std::io::Error::other("Sign-in window closed.").into());
        };
        *result = self.finish(handler, next, |handler, runtime| handler.on_closed(session, runtime))?;
        Ok(())
    }

    fn finish<T, F>(
        &mut self,
        handler: &mut T,
        next: &mut u64,
        callback: F,
    ) -> HandlerResult<Option<T::Output>>
    where
        T: SessionHandler,
        F: FnOnce(&mut T, &mut RuntimeCommands) -> HandlerResult<HandlerControl<T::Output>>,
    {
        let mut commands = RuntimeCommands::new(*next);
        let control = callback(handler, &mut commands)?;
        *next = commands.next_session_id;
        self.apply(&mut commands)?;
        match control {
            HandlerControl::Complete(value) => Ok(Some(value)),
            HandlerControl::Continue => Ok(None),
        }
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        unsafe {
            TerminateProcess(self.0.as_raw_handle(), 1);
            WaitForSingleObject(self.0.as_raw_handle(), 5_000);
        }
    }
}

impl Cdp {
    fn attach(&mut self) -> HandlerResult<()> {
        let targets = self.call("Target.getTargets", json!({}), None, CALL_TIMEOUT)?;
        let target = targets["targetInfos"]
            .as_array()
            .and_then(|items| {
                items.iter().find(|item| item["type"] == "page" && item["url"] == "about:blank")
            })
            .ok_or_else(|| std::io::Error::other("Sign-in window is unavailable."))?;
        let target_id = target["targetId"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("Sign-in window is unavailable."))?
            .to_string();
        self.attach_target(&target_id)
    }

    fn attach_target(&mut self, target_id: &str) -> HandlerResult<()> {
        self.target_id = target_id.to_string();
        let attached = self.call(
            "Target.attachToTarget",
            json!({ "targetId": self.target_id, "flatten": true }),
            None,
            CALL_TIMEOUT,
        )?;
        self.page_session = attached["sessionId"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("Sign-in window is unavailable."))?
            .to_string();
        self.call("Target.setDiscoverTargets", json!({ "discover": true }), None, CALL_TIMEOUT)?;
        self.call("Page.enable", json!({}), Some(self.page_session.clone()), CALL_TIMEOUT)?;
        let tree = self.call("Page.getFrameTree", json!({}), Some(self.page_session.clone()), CALL_TIMEOUT)?;
        self.main_frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("Sign-in window is unavailable."))?
            .to_string();
        self.call("Runtime.enable", json!({}), Some(self.page_session.clone()), CALL_TIMEOUT)?;
        let binding = format!("nexusAuth{}", uuid::Uuid::new_v4().simple());
        self.call(
            "Runtime.addBinding",
            json!({ "name": binding }),
            Some(self.page_session.clone()),
            CALL_TIMEOUT,
        )?;
        self.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": bridge_source(&binding) }),
            Some(self.page_session.clone()),
            CALL_TIMEOUT,
        )?;
        let _ = self.call(
            "Browser.setDownloadBehavior",
            json!({ "behavior": "deny" }),
            None,
            CALL_TIMEOUT,
        );
        let patterns = TRUSTED
            .iter()
            .map(|host| json!({ "urlPattern": format!("https://{host}/*"), "requestStage": "Request" }))
            .collect::<Vec<_>>();
        self.call(
            "Fetch.enable",
            json!({ "patterns": patterns }),
            Some(self.page_session.clone()),
            CALL_TIMEOUT,
        )?;
        Ok(())
    }

    fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<String>,
        timeout: Duration,
    ) -> HandlerResult<Value> {
        let id = self.send(method, params, session.as_deref())?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(result) = self.ready.remove(&id) {
                return result.map_err(|error| std::io::Error::other(error).into());
            }
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            if wait.is_zero() {
                return Err(std::io::Error::other("Sign-in browser did not respond.").into());
            }
            match self.incoming.recv_timeout(wait) {
                Ok(Ok(message)) => self.ingest(message)?,
                Ok(Err(error)) => return Err(std::io::Error::other(error).into()),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(std::io::Error::other("Sign-in browser did not respond.").into());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(std::io::Error::other("Sign-in window closed.").into());
                }
            }
        }
    }

    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> HandlerResult<u64> {
        self.next += 1;
        let id = self.next;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(0);
        self.stdin.write_all(&bytes)?;
        self.stdin.flush()?;
        Ok(id)
    }

    fn ingest(&mut self, message: Value) -> HandlerResult<()> {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            if self.ignore.remove(&id) {
                return Ok(());
            }
            let result = if message.get("error").is_some() {
                Err(browser_command_error(&message))
            } else {
                Ok(message.get("result").cloned().unwrap_or_else(|| json!({})))
            };
            self.ready.insert(id, result);
            return Ok(());
        }
        if message.get("method").and_then(Value::as_str) == Some("Fetch.requestPaused") {
            self.continue_request(&message)?;
            return Ok(());
        }
        if message.get("method").is_some() {
            self.events.push_back(message);
        }
        Ok(())
    }

    fn continue_request(&mut self, event: &Value) -> HandlerResult<()> {
        let params = &event["params"];
        let request_id = params["requestId"].clone();
        let mut body = json!({ "requestId": request_id });
        if let Some(headers) = override_headers(params, self.main_frame.as_str(), self.document_url.as_deref(), &self.document_headers) {
            body["headers"] = Value::Array(headers);
        }
        let session = event.get("sessionId").and_then(Value::as_str).map(str::to_string);
        let id = self.send("Fetch.continueRequest", body, session.as_deref())?;
        self.ignore.insert(id);
        Ok(())
    }

    fn evaluate(&mut self, context_id: i64, expression: &str) -> HandlerResult<Value> {
        let value = self.call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "contextId": context_id,
                "returnByValue": true
            }),
            Some(self.page_session.clone()),
            CALL_TIMEOUT,
        )?;
        if value.get("exceptionDetails").is_some() {
            return Err(std::io::Error::other("Sign-in page failed.").into());
        }
        Ok(value)
    }

    fn navigate(&mut self, url: &str) -> HandlerResult<()> {
        let result = self.call(
            "Page.navigate",
            json!({ "url": url }),
            Some(self.page_session.clone()),
            CALL_TIMEOUT,
        )?;
        if let Some(text) = result.get("errorText").and_then(Value::as_str) {
            return Err(std::io::Error::other(format!("Sign-in page did not load: {}", scrub_log_text(text))).into());
        }
        Ok(())
    }

    fn navigate_in_new_target(&mut self, url: &str) -> HandlerResult<()> {
        let created = self.call("Target.createTarget", json!({ "url": "about:blank" }), None, CALL_TIMEOUT)?;
        let target_id = created["targetId"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("Sign-in window is unavailable."))?
            .to_string();
        let previous = self.target_id.clone();
        self.attach_target(&target_id)?;
        if !previous.is_empty() && previous != target_id {
            let _ = self.call(
                "Target.closeTarget",
                json!({ "targetId": previous }),
                None,
                Duration::from_secs(3),
            );
        }
        self.navigate(url)
    }
}

fn override_headers(
    paused: &Value,
    main_frame: &str,
    document_url: Option<&str>,
    frame_headers: &[(String, String)],
) -> Option<Vec<Value>> {
    let document_url = document_url?;
    if paused["frameId"].as_str() != Some(main_frame)
        || paused["resourceType"].as_str() != Some("Document")
        || paused["request"]["url"].as_str() != Some(document_url)
        || !trusted_auth_url(document_url)
    {
        return None;
    }
    let mut names = HashSet::new();
    for (name, _) in frame_headers {
        names.insert(name.to_ascii_lowercase());
    }
    let mut headers = Vec::new();
    if let Some(existing) = paused["request"]["headers"].as_object() {
        for (name, value) in existing {
            if names.contains(&name.to_ascii_lowercase()) {
                continue;
            }
            if let Some(value) = value.as_str() {
                headers.push(json!({ "name": name, "value": value }));
            }
        }
    }
    for (name, value) in frame_headers {
        headers.push(json!({ "name": name, "value": value }));
    }
    Some(headers)
}

fn trusted_auth_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://") else {
        return false;
    };
    if rest.starts_with('/') || rest.contains('@') {
        return false;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && !host.contains(':') && TRUSTED.contains(&host)
}

fn trusted_origin(origin: &str) -> bool {
    TRUSTED.iter().any(|host| origin == format!("https://{host}"))
}

fn bridge_source(binding: &str) -> String {
    let binding = serde_json::to_string(binding).unwrap_or_else(|_| "\"\"".to_string());
    let hosts = serde_json::to_string(&TRUSTED).unwrap_or_else(|_| "[]".to_string());
    format!(
        r#"(function(name, hosts) {{
  if (window !== window.top || location.protocol !== "https:" || location.port || hosts.indexOf(location.hostname) < 0) return;
  var send = function(payload) {{
    if (typeof payload === "string" && payload.length <= 65536) globalThis[name](payload);
  }};
  var note = function(event, extra) {{
    var message = {{ type: "diagnostic", event: event, host: location.hostname, path: location.pathname }};
    if (extra) for (var key in extra) message[key] = extra[key];
    send(JSON.stringify(message));
  }};
  var slimToken = function(data) {{
    if (!data || typeof data.sDAToken !== "string" || !data.sDAToken) return "";
    return JSON.stringify({{
      sDAToken: data.sDAToken,
      sDASessionKey: data.sDASessionKey,
      sDAStartTime: data.sDAStartTime,
      sDAExpires: data.sDAExpires,
      sSTSInlineFlowToken: data.sSTSInlineFlowToken,
      sSigninName: data.sSigninName,
      K: data.K
    }});
  }};
  var notify = function(payload) {{
    if (typeof payload !== "string") return;
    if (payload.indexOf("\"sDAToken\"") !== -1) {{
      try {{
        var slim = slimToken(JSON.parse(payload));
        if (slim) {{ note("token", {{ length: payload.length, slim: slim.length }}); send(slim); return; }}
        note("token-empty", {{ length: payload.length }});
      }} catch (e) {{ note("token-parse", {{ length: payload.length }}); }}
      return;
    }}
    if (payload.length > 65536) {{ note("dropped", {{ length: payload.length }}); return; }}
    send(payload);
  }};
  var installed = false;
  try {{
    Object.defineProperty(window.external, "notify", {{ value: notify, configurable: true, writable: true }});
    installed = window.external.notify === notify;
  }} catch (e) {{}}
  if (!installed) {{
    try {{ window.external = {{ notify: notify }}; installed = window.external.notify === notify; }} catch (e2) {{}}
  }}
  note(installed ? "bridge-ready" : "notify-install-failed");
  var report = function() {{
    if (location.hostname !== "login.live.com" || location.pathname !== "/ppsecure/post.srf") return;
    var slim = slimToken(window.ServerData);
    if (!slim) {{ note("post-without-token"); return; }}
    note("server-data", {{ length: slim.length }});
    send(slim);
  }};
  window.addEventListener("DOMContentLoaded", report);
  window.addEventListener("load", report);
}})({binding}, {hosts});"#
    )
}

fn context_is_gone(message: &str) -> bool {
    message.contains("Cannot find context") || message.contains("Cannot find execution context")
}

fn clean_page_url(url: &str) -> String {
    url.chars().filter(|character| !character.is_whitespace()).collect()
}

fn browser_command_error(message: &Value) -> String {
    let code = message.pointer("/error/code").and_then(Value::as_i64).unwrap_or(0);
    let text = message
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("command failed");
    format!("browser command {code}: {}", scrub_log_text(text))
}

fn scrub_log_text(text: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;
    while let Some(index) = remaining.find("://") {
        let scheme = remaining[..index]
            .rfind(char::is_whitespace)
            .map(|offset| offset + 1)
            .unwrap_or(0);
        result.push_str(&remaining[..scheme]);
        let url = &remaining[scheme..];
        let end = url.find(char::is_whitespace).unwrap_or(url.len());
        result.push_str(&page_location(&url[..end]));
        remaining = &url[end..];
    }
    result.push_str(remaining);
    if result.len() > 240 {
        result.truncate(240);
    }
    result
}

fn trace_path() -> PathBuf {
    std::env::temp_dir().join("xodus-login-trace.txt")
}

fn sign_in_log(message: &str) {
    eprintln!("sign-in: {message}");
    let _ = std::io::stderr().flush();
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(trace_path()) {
        let _ = writeln!(file, "sign-in: {message}");
    }
}

fn page_location(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return url.split(['?', '#']).next().unwrap_or(url).to_string();
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let path = rest
        .find('/')
        .map(|index| rest[index..].split(['?', '#']).next().unwrap_or("/"))
        .unwrap_or("/");
    format!("{host}{path}")
}

fn log_diagnostic(payload: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("diagnostic") {
        return false;
    }
    let event = value.get("event").and_then(Value::as_str).unwrap_or("message");
    let host = value.get("host").and_then(Value::as_str).unwrap_or("");
    let path = value.get("path").and_then(Value::as_str).unwrap_or("");
    let length = value.get("length").and_then(Value::as_u64);
    let slim = value.get("slim").and_then(Value::as_u64);
    sign_in_log(&format!("{event} host={host} path={path} length={length:?} slim={slim:?}"));
    true
}

fn payload_keys(payload: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return format!("bytes={}", payload.len());
    };
    let Some(object) = value.as_object() else {
        return format!("bytes={}", payload.len());
    };
    let mut keys = object.keys().cloned().collect::<Vec<_>>();
    keys.truncate(12);
    format!("keys={}", keys.join(","))
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<StartupInfoW>() == 104);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<StartupInfoExW>() == 112);

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_count_chars: u32,
    y_count_chars: u32,
    fill_attribute: u32,
    flags: u32,
    show_window: u16,
    cb_reserved2: u16,
    reserved2: *mut u8,
    std_input: *mut c_void,
    std_output: *mut c_void,
    std_error: *mut c_void,
}

#[repr(C)]
struct StartupInfoExW {
    startup_info: StartupInfoW,
    attribute_list: *mut c_void,
}

#[repr(C)]
struct ProcessInformation {
    process: *mut c_void,
    thread: *mut c_void,
    process_id: u32,
    thread_id: u32,
}

struct PipeEnds {
    read: *mut c_void,
    write: *mut c_void,
}

impl Drop for PipeEnds {
    fn drop(&mut self) {
        unsafe {
            if !self.read.is_null() {
                CloseHandle(self.read);
            }
            if !self.write.is_null() {
                CloseHandle(self.write);
            }
        }
    }
}

impl PipeEnds {
    fn new() -> std::io::Result<Self> {
        let mut ends = Self {
            read: std::ptr::null_mut(),
            write: std::ptr::null_mut(),
        };
        let security = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            descriptor: std::ptr::null_mut(),
            inherit: 1,
        };
        let created = unsafe { CreatePipe(&mut ends.read, &mut ends.write, &security, 0) };
        if created == 0 {
            return Err(std::io::Error::other(format!(
                "Sign-in browser did not start: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(ends)
    }
}

fn spawn_browser(browser: &Browser, profile: &Path) -> std::io::Result<(BrowserProcess, File, File)> {
    let mut to_browser = PipeEnds::new()?;
    let mut from_browser = PipeEnds::new()?;
    // Only the ends the browser adopts stay inheritable.
    unsafe {
        SetHandleInformation(to_browser.write, 1, 0);
        SetHandleInformation(from_browser.read, 1, 0);
    }
    let chrome_read = to_browser.read as usize as u32;
    let chrome_write = from_browser.write as usize as u32;
    let application = wide_null(browser.path.as_os_str());
    let command = command_line(browser, profile, chrome_read, chrome_write);
    let inherited = [to_browser.read, from_browser.write];
    let mut info = ProcessInformation {
        process: std::ptr::null_mut(),
        thread: std::ptr::null_mut(),
        process_id: 0,
        thread_id: 0,
    };
    if !create_with_handle_list(&application, &command, &inherited, &mut info) {
        let mut startup: StartupInfoExW = unsafe { std::mem::zeroed() };
        startup.startup_info.cb = std::mem::size_of::<StartupInfoW>() as u32;
        let mut fallback = command.clone();
        let started = unsafe {
            CreateProcessW(
                application.as_ptr(),
                fallback.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                1,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut startup,
                &mut info,
            )
        };
        if started == 0 {
            return Err(std::io::Error::other(format!(
                "Sign-in browser did not start: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    unsafe {
        CloseHandle(to_browser.read);
        to_browser.read = std::ptr::null_mut();
        CloseHandle(from_browser.write);
        from_browser.write = std::ptr::null_mut();
        CloseHandle(info.thread);
    }
    let stdout = unsafe { File::from(OwnedHandle::from_raw_handle(from_browser.read)) };
    from_browser.read = std::ptr::null_mut();
    let stdin = unsafe { File::from(OwnedHandle::from_raw_handle(to_browser.write)) };
    to_browser.write = std::ptr::null_mut();
    let process = unsafe { OwnedHandle::from_raw_handle(info.process) };
    Ok((BrowserProcess(process), stdout, stdin))
}

fn create_with_handle_list(
    application: &[u16],
    command: &[u16],
    handles: &[*mut c_void],
    info: &mut ProcessInformation,
) -> bool {
    let mut size = 0usize;
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
    if size == 0 {
        return false;
    }
    let mut words = vec![0u64; size.div_ceil(8)];
    let list = words.as_mut_ptr() as *mut c_void;
    let initialized = unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) };
    if initialized == 0 {
        return false;
    }
    let updated = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            0x0002_0002,
            handles.as_ptr() as *mut c_void,
            std::mem::size_of_val(handles),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    let started = if updated == 0 {
        0
    } else {
        let mut startup: StartupInfoExW = unsafe { std::mem::zeroed() };
        startup.startup_info.cb = std::mem::size_of::<StartupInfoExW>() as u32;
        startup.attribute_list = list;
        let mut command = command.to_vec();
        unsafe {
            CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                1,
                0x0008_0000,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut startup,
                info,
            )
        }
    };
    unsafe { DeleteProcThreadAttributeList(list) };
    started != 0
}

/// `--do-not-de-elevate`: started from an elevated process, current Chrome and
/// Edge relaunch themselves without administrator rights and the first process
/// exits, so the relaunched browser never has the DevTools pipe.
fn command_line(browser: &Browser, profile: &Path, read: u32, write: u32) -> Vec<u16> {
    let mut line = Vec::new();
    line.push(u16::from(b'"'));
    line.extend(browser.path.as_os_str().encode_wide());
    line.push(u16::from(b'"'));
    let rest = format!(
        " --user-data-dir=\"{}\" --do-not-de-elevate --incognito --no-first-run --no-default-browser-check --disable-sync --disable-extensions --remote-debugging-pipe --remote-debugging-io-pipes={read},{write} --window-size=480,720 about:blank",
        profile.display()
    );
    line.extend(rest.encode_utf16());
    line.push(0);
    line
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreatePipe(
        read: *mut *mut c_void,
        write: *mut *mut c_void,
        security: *const SecurityAttributes,
        size: u32,
    ) -> i32;
    fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn InitializeProcThreadAttributeList(
        list: *mut c_void,
        count: u32,
        flags: u32,
        size: *mut usize,
    ) -> i32;
    fn UpdateProcThreadAttribute(
        list: *mut c_void,
        flags: u32,
        attribute: usize,
        value: *mut c_void,
        size: usize,
        previous: *mut c_void,
        returned: *mut usize,
    ) -> i32;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn CreateProcessW(
        application: *const u16,
        command: *mut u16,
        process_attributes: *mut c_void,
        thread_attributes: *mut c_void,
        inherit: i32,
        flags: u32,
        environment: *mut c_void,
        directory: *const u16,
        startup: *mut StartupInfoExW,
        info: *mut ProcessInformation,
    ) -> i32;
    fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
}

fn read_pipe(stdout: impl std::io::Read, tx: mpsc::Sender<Result<Value, String>>) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(0, &mut buffer) {
            Ok(0) => {
                let _ = tx.send(Err("Sign-in window closed.".to_string()));
                break;
            }
            Ok(_) => {
                if buffer.last() == Some(&0) {
                    buffer.pop();
                }
                if buffer.len() > 2 * 1024 * 1024 {
                    let _ = tx.send(Err("Sign-in browser response was too large.".to_string()));
                    break;
                }
                match serde_json::from_slice::<Value>(&buffer) {
                    Ok(value) => {
                        if tx.send(Ok(value)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(Err("Sign-in browser sent an invalid response.".to_string()));
                        break;
                    }
                }
            }
            Err(_) => {
                let _ = tx.send(Err("Sign-in window closed.".to_string()));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_urls_stay_on_microsoft_hosts() {
        assert!(trusted_auth_url(
            "https://login.live.com/ppsecure/InlineLogin.srf?id=80604"
        ));
        assert!(trusted_auth_url("https://account.live.com/path"));
        assert!(!trusted_auth_url("http://login.live.com/"));
        assert!(!trusted_auth_url("https://login.live.com:443/"));
        assert!(!trusted_auth_url("https://user:pass@login.live.com/"));
        assert!(!trusted_auth_url("https://evil.example/"));
        assert!(trusted_origin("https://login.live.com"));
        assert!(!trusted_origin("https://login.live.com/extra"));
    }

    #[test]
    fn broker_headers_are_added_only_to_the_login_document() {
        let paused = json!({
            "frameId": "main",
            "resourceType": "Document",
            "request": {
                "url": "https://login.live.com/ppsecure/InlineLogin.srf",
                "headers": { "User-Agent": "Edge", "cxh-protocol": "old" }
            }
        });
        let frame = vec![("cxh-protocol".to_string(), "TokenBroker".to_string())];
        let headers = override_headers(
            &paused,
            "main",
            Some("https://login.live.com/ppsecure/InlineLogin.srf"),
            &frame,
        )
        .unwrap();
        assert!(headers.iter().any(|header| header["name"] == "User-Agent" && header["value"] == "Edge"));
        assert!(headers.iter().any(|header| header["name"] == "cxh-protocol" && header["value"] == "TokenBroker"));
        assert_eq!(headers.iter().filter(|header| header["name"] == "cxh-protocol").count(), 1);
        assert!(override_headers(&paused, "main", Some("https://login.live.com/other"), &frame).is_none());
        assert!(override_headers(
            &json!({ "frameId": "child", "resourceType": "Document", "request": paused["request"].clone() }),
            "main",
            Some("https://login.live.com/ppsecure/InlineLogin.srf"),
            &frame,
        )
        .is_none());
    }

    #[test]
    fn a_browser_with_an_update_waiting_is_noticed() {
        let root = tempfile::tempdir().unwrap();
        let application = root.path().join("Google").join("Chrome").join("Application");
        std::fs::create_dir_all(application.join("156.0.1.2")).unwrap();
        let browser = Browser { path: application.join("chrome.exe"), label: "Google Chrome" };
        std::fs::write(&browser.path, b"").unwrap();
        assert!(!update_pending(&browser));
        std::fs::write(application.join("new_chrome.exe"), b"").unwrap();
        assert!(update_pending(&browser));
        let versioned = Browser { path: application.join("156.0.1.2").join("chrome.exe"), label: "Google Chrome" };
        assert!(update_pending(&versioned));
    }

    #[test]
    fn devtools_pipe_reaches_a_blank_page() {
        let Some(browser) = find_browser() else {
            return;
        };
        let host = Host::launch(&browser).expect("browser pipe");
        assert!(!host.cdp.page_session.is_empty());
        assert!(!host.cdp.target_id.is_empty());
    }

    #[test]
    fn bridge_installs_notify_without_changing_webauthn() {
        let source = bridge_source("nexusAuthTest");
        assert!(source.contains("window.external"));
        assert!(source.contains("sDAToken"));
        assert!(source.contains("sSTSInlineFlowToken"));
        assert!(source.contains("notify-install-failed"));
        assert!(!source.contains("credentials.create"));
        assert!(!source.contains("navigator.credentials"));
    }
}

