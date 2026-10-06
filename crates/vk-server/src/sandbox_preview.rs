//! Previews out of container boxes (13 §4, §14 acceptance 2): a box runs with `--network none`
//! and publishes no ports, so a dev server inside it is reached over the box link's `tcp:<port>`
//! channel. For each box port the server keeps one host listener on `127.0.0.1` (the same port
//! number when it is free, else an ephemeral one) whose connections become `tcp:` channels; the
//! preview record points at that host port, so `*.vibeke.localhost` URLs, the session proxy and
//! screenshots work unchanged.
//!
//! Ports get there two ways: `preview.declare` through a box pane's broker is rewritten to the
//! forwarded host port, and listening sockets inside linked boxes are discovered every few
//! seconds (`/proc/net/tcp*`, `[isolation.container] discover_ports`).

use super::*;
use std::collections::HashSet;

#[derive(Default)]
pub struct FwdState {
    inner: Mutex<HashMap<(String, u16), Fwd>>,
}

struct Fwd {
    host_port: u16,
    handle: JoinHandle<()>,
    /// Discovered (not declared): dropped when the box stops listening.
    discovered: bool,
}

impl Drop for Fwd {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// The host port forwarding box port `port` of box `key`, if any.
pub fn host_port(server: &Server, key: &str, port: u16) -> Option<u16> {
    server
        .sandbox
        .forwards
        .inner
        .lock()
        .unwrap()
        .get(&(key.to_string(), port))
        .map(|f| f.host_port)
}

/// Forwards of box `key`: `(box port, host port)`.
pub fn list(server: &Server, key: &str) -> Vec<(u16, u16)> {
    let mut v: Vec<(u16, u16)> = server
        .sandbox
        .forwards
        .inner
        .lock()
        .unwrap()
        .iter()
        .filter(|((k, _), _)| k == key)
        .map(|((_, p), f)| (*p, f.host_port))
        .collect();
    v.sort_unstable();
    v
}

/// Stop every forward of box `key` (teardown).
pub fn stop_all(server: &Server, key: &str) {
    server
        .sandbox
        .forwards
        .inner
        .lock()
        .unwrap()
        .retain(|(k, _), _| k != key);
}

/// Listen on the host for box port `port` of box `key` (idempotent). Each accepted connection
/// opens a `tcp:<port>` channel on the box link.
pub async fn ensure(
    server: &Arc<Server>,
    key: &str,
    port: u16,
    discovered: bool,
) -> Result<u16, String> {
    if let Some(h) = host_port(server, key, port) {
        if !discovered
            && let Some(f) = server
                .sandbox
                .forwards
                .inner
                .lock()
                .unwrap()
                .get_mut(&(key.to_string(), port))
        {
            f.discovered = false;
        }
        return Ok(h);
    }
    if link(server, key).is_none() {
        return Err("the box has no link (the static Linux vibeke binary is needed inside)".into());
    }
    let l = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| e.to_string())?,
    };
    let host_port = l.local_addr().map_err(|e| e.to_string())?.port();
    let weak = Arc::downgrade(server);
    let k = key.to_string();
    let handle = tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let Some(srv) = weak.upgrade() else { return };
            let Some(link) = link(&srv, &k) else { continue };
            tokio::spawn(async move {
                if let Ok(mut ch) = link.open_tcp(port).await {
                    let _ = s.set_nodelay(true);
                    let _ = tokio::io::copy_bidirectional(&mut s, &mut ch).await;
                }
            });
        }
    });
    let mut m = server.sandbox.forwards.inner.lock().unwrap();
    // Raced with another caller: keep the first listener.
    if let Some(f) = m.get(&(key.to_string(), port)) {
        handle.abort();
        return Ok(f.host_port);
    }
    m.insert(
        (key.to_string(), port),
        Fwd {
            host_port,
            handle,
            discovered,
        },
    );
    Ok(host_port)
}

/// The box (key) a contained pane's broker belongs to, when it is a linked container box.
fn container_box_of_pane(server: &Server, pane: &str) -> Option<Arc<TaskBox>> {
    let (level, ws_task) = server.with_core(|c| {
        let p = c.pane(pane)?;
        Some((
            p.isolation.level,
            c.ws(&p.workspace).and_then(|w| w.task.clone()),
        ))
    })?;
    if level != IsolationLevel::Container {
        return None;
    }
    let key = ws_task.unwrap_or_else(|| format!("pane:{pane}"));
    server
        .sandbox
        .get(&key)
        .filter(|b| matches!(b.runner, BoxRunner::Container(_)))
}

/// `preview.declare` through the broker of a container pane: the port the agent names is a box
/// port; the preview gets the host port that forwards it. Returns the rewritten params (or the
/// original ones when the pane is not in a linked container box).
pub async fn rewrite_declare(server: &Arc<Server>, pane: &str, mut params: Value) -> Value {
    let Some(b) = container_box_of_pane(server, pane) else {
        return params;
    };
    let port = params
        .get("port")
        .and_then(Value::as_u64)
        .or_else(|| s(&params, "port").and_then(|x| x.parse().ok()))
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0);
    let Some(port) = port else { return params };
    match ensure(server, &b.key, port, false).await {
        Ok(h) => {
            if let Some(o) = params.as_object_mut() {
                o.insert("port".into(), json!(h));
                o.entry("label")
                    .or_insert_with(|| json!(format!("box:{port}")));
            }
            emit(
                server,
                "sandbox.port_forwarded",
                json!({"task": b.task, "sandbox": b.key}),
                json!({"box_port": port, "host_port": h, "source": "declared"}),
            );
        }
        Err(e) => tracing::info!(sandbox = %b.key, port, error = %e, "box preview not forwarded"),
    }
    params
}

/// Discovery pass over linked container boxes (from the extras tick).
pub async fn poll_ports(server: &Arc<Server>, boxes: &[Arc<TaskBox>]) {
    let linked: Vec<Arc<TaskBox>> = boxes
        .iter()
        .filter(|b| {
            link(server, &b.key)
                .is_some_and(|l| l.connected.load(std::sync::atomic::Ordering::SeqCst))
        })
        .cloned()
        .collect();
    if linked.is_empty() {
        return;
    }
    let scanned = tokio::task::spawn_blocking(move || {
        linked
            .into_iter()
            .map(|b| {
                let ports = match &b.runner {
                    BoxRunner::Container(c) => c.b().listening_ports(),
                    _ => vec![],
                };
                (b, ports)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    for (b, ports) in scanned {
        observe_ports(server, &b, &ports).await;
    }
}

/// Ports the in-box link itself listens on (egress forwarder) are never previews.
fn infra_port(p: u16) -> bool {
    p == vk_sandbox::container::BOX_PROXY_PORT
}

/// Apply one discovery result for box `b` (also the test hook): forward new ports and declare
/// them as previews of the task, drop discovered forwards whose port went away.
pub async fn observe_ports(server: &Arc<Server>, b: &Arc<TaskBox>, ports: &[u16]) {
    let now: HashSet<u16> = ports
        .iter()
        .copied()
        .filter(|p| !infra_port(*p) && *p >= 1024)
        .collect();
    // Gone: discovered forwards only (declared ones stay until teardown).
    server
        .sandbox
        .forwards
        .inner
        .lock()
        .unwrap()
        .retain(|(k, p), f| k != &b.key || !f.discovered || now.contains(p));
    let task_pane = server.with_core(|c| {
        let ws = b
            .task
            .as_ref()
            .and_then(|t| c.task(t))
            .and_then(|t| t.workspace.clone());
        c.model
            .panes
            .iter()
            .find(|p| ws.as_deref() == Some(p.workspace.as_str()))
            .map(|p| p.id.clone())
    });
    for port in now {
        if host_port(server, &b.key, port).is_some() {
            continue;
        }
        let h = match ensure(server, &b.key, port, true).await {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(sandbox = %b.key, port, error = %e, "box port not forwarded");
                continue;
            }
        };
        let mut q = json!({"port": h, "label": format!("box:{port}")});
        if let Some(t) = &b.task {
            q["task"] = json!(t);
        }
        if let Some(pn) = &task_pane {
            q["pane"] = json!(pn);
        }
        let ctx = crate::drafts::user_ctx();
        if let Err(e) = crate::preview::declare(server, &ctx, &q) {
            tracing::debug!(sandbox = %b.key, port, error = %e.message, "box preview not declared");
        }
        emit(
            server,
            "sandbox.port_forwarded",
            json!({"task": b.task, "sandbox": b.key}),
            json!({"box_port": port, "host_port": h, "source": "discovered"}),
        );
    }
}
