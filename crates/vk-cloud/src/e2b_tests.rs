use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::SinkExt;
use serde_json::{Value, json};

use super::*;

const KEY: &str = "e2b_good";
const NOCARD: &str = "e2b_nocard";
const ENVD_TOKEN: &str = "envd-tok";
const HOST: &str = "0123abcd";
const BOXKEY: &str = "0123456789";

/// Calls the mock saw, in order: (name, body).
#[derive(Default)]
struct Mock {
    calls: std::sync::Mutex<Vec<(String, Value)>>,
}

impl Mock {
    fn record(&self, name: &str, v: Value) {
        self.calls.lock().unwrap().push((name.to_string(), v));
    }
    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
    fn find(&self, name: &str) -> Option<Value> {
        self.calls()
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }
    async fn wait_for(&self, names: &[&str]) {
        for _ in 0..1000 {
            let calls = self.calls();
            if names.iter().all(|n| calls.iter().any(|(c, _)| c == n)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the mock never saw {names:?}");
    }
}

type M = State<Arc<Mock>>;

fn auth(h: &HeaderMap) -> std::result::Result<(), (StatusCode, Json<Value>)> {
    let k = h
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if k == NOCARD {
        return Err((
            StatusCode::FORBIDDEN,
            Json(
                json!({"code": 403, "message": "Add a credit card in billing to start sandboxes"}),
            ),
        ));
    }
    if k != KEY {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"code": 401, "message": "Invalid API key"})),
        ));
    }
    Ok(())
}

fn listed(id: &str, state: &str, meta: Value) -> Value {
    json!({
        "templateID": "base", "sandboxID": id, "clientID": "c", "startedAt": "2026-01-02T03:04:05Z",
        "endAt": "2026-01-02T04:04:05Z", "cpuCount": 2, "memoryMB": 512, "diskSizeMB": 1024,
        "metadata": meta, "state": state, "envdVersion": "0.5.7"
    })
}

fn vk_meta(key: &str) -> Value {
    json!({"vibeke_host": HOST, "vibeke_key": key, "vibeke_name": format!("vk-{HOST}-{key}")})
}

async fn list(h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Err(e) = auth(&h) {
        return e.into_response();
    }
    assert!(q.contains_key("limit"), "{q:?}");
    if q.get("nextToken").map(String::as_str) == Some("p2") {
        return Json(json!([listed("sbx3", "paused", vk_meta("aaaaaaaaaa"))])).into_response();
    }
    (
        [("x-next-token", "p2")],
        Json(json!([
            listed("sbx1", "running", vk_meta(BOXKEY)),
            listed("foreign", "running", json!({"owner": "someone"})),
            listed(
                "badtags",
                "running",
                json!({"vibeke_host": "zz", "vibeke_key": BOXKEY})
            ),
        ])),
    )
        .into_response()
}

fn sandbox(id: &str) -> Value {
    json!({
        "templateID": "base", "sandboxID": id, "clientID": "c", "envdVersion": "0.5.7",
        "envdAccessToken": ENVD_TOKEN, "domain": "e2b.test"
    })
}

async fn create(State(m): M, h: HeaderMap, body: String) -> Response {
    if let Err(e) = auth(&h) {
        return e.into_response();
    }
    let v: Value = serde_json::from_str(&body).unwrap();
    m.record("create", v);
    (StatusCode::CREATED, Json(sandbox("sbx1"))).into_response()
}

async fn get_one(h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(e) = auth(&h) {
        return e.into_response();
    }
    // Ids starting with "paused" are paused sandboxes.
    let state = if id.starts_with("paused") {
        "paused"
    } else {
        "running"
    };
    let mut v = listed(&id, state, vk_meta(BOXKEY));
    v["envdAccessToken"] = json!(ENVD_TOKEN);
    v["domain"] = json!("e2b.test");
    Json(v).into_response()
}

async fn delete_one(h: HeaderMap, Path(_id): Path<String>) -> Response {
    if let Err(e) = auth(&h) {
        return e.into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"code": 404, "message": "sandbox not found"})),
    )
        .into_response()
}

async fn pause(h: HeaderMap, Path(_id): Path<String>) -> Response {
    if let Err(e) = auth(&h) {
        return e.into_response();
    }
    (
        StatusCode::CONFLICT,
        Json(json!({"code": 409, "message": "sandbox is already paused"})),
    )
        .into_response()
}

async fn connect(State(m): M, h: HeaderMap, Path(id): Path<String>, body: String) -> Response {
    if let Err(e) = auth(&h) {
        return e.into_response();
    }
    m.record(
        "connect",
        serde_json::from_str(&body).unwrap_or(Value::Null),
    );
    Json(sandbox(&id)).into_response()
}

async fn snapshot(h: HeaderMap, Path(_id): Path<String>) -> Response {
    if let Err(e) = auth(&h) {
        return e.into_response();
    }
    (
        StatusCode::CREATED,
        Json(json!({"snapshotID": "snap1:default", "names": ["team/snap1:default"]})),
    )
        .into_response()
}

fn header<'a>(h: &'a HeaderMap, k: &str) -> &'a str {
    h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn frame(v: Value) -> std::result::Result<Bytes, std::io::Error> {
    Ok(Bytes::from(envelope(0, v.to_string().as_bytes())))
}

fn data(kind: &str, d: &[u8]) -> Value {
    json!({"event": {"data": {kind: b64(d)}}})
}

async fn rpc(State(m): M, h: HeaderMap, Path(method): Path<String>, body: Bytes) -> Response {
    if header(&h, "x-access-token") != ENVD_TOKEN {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code": "unauthenticated", "message": "bad access token"})),
        )
            .into_response();
    }
    assert_eq!(header(&h, "connect-protocol-version"), "1");
    assert_eq!(header(&h, "e2b-sandbox-port"), "49983");
    if method == "Start" || method == "Connect" {
        assert_eq!(header(&h, "content-type"), "application/connect+json");
        assert_eq!(header(&h, "keepalive-ping-interval"), "50");
        assert_eq!(header(&h, "authorization"), "Basic dXNlcjo=");
        let mut f = Frames::default();
        f.push(&body);
        let (flags, msg) = f.next_frame().unwrap().expect("one request envelope");
        assert_eq!(flags, 0);
        let v: Value = serde_json::from_slice(&msg).unwrap();
        m.record(&method, v.clone());
        return stream(m, method, v);
    }
    assert_eq!(header(&h, "content-type"), "application/json");
    let v: Value = serde_json::from_slice(&body).unwrap();
    m.record(&method, v.clone());
    if method == "SendInput" && v["process"]["pid"] == 66 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"code": "unavailable", "message": "input dropped"})),
        )
            .into_response();
    }
    let resp = match method.as_str() {
        "List" => {
            let mut procs = vec![
                json!({"pid": 42, "config": {"cmd": "bash", "args": ["-l"], "envs": {}}, "tag": "vk-tty-abc"}),
                json!({"pid": 7, "config": {"cmd": "git", "args": ["status"]}, "tag": "vk-pipe-def"}),
                json!({"pid": 9, "config": {"cmd": "other"}}),
            ];
            // A `flaky` Start lost its response, but the process runs.
            for (n, c) in m.calls() {
                if n == "Start" && c["process"]["cmd"] == "flaky" {
                    procs.push(json!({"pid": 99, "config": {"cmd": "flaky"}, "tag": c["tag"]}));
                }
            }
            json!({ "processes": procs })
        }
        _ => json!({}),
    };
    Json(resp).into_response()
}

/// A process stream scripted by the request.
fn stream(m: Arc<Mock>, method: String, req: Value) -> Response {
    let (mut tx, rx) =
        futures::channel::mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        if method == "Connect" {
            let pid = req["process"]["pid"].as_u64().unwrap();
            tx.send(frame(json!({"event": {"start": {"pid": pid}}})))
                .await
                .unwrap();
            tx.send(frame(data("pty", b"replay"))).await.unwrap();
            // The stream drops without an end event.
            return;
        }
        let cmd = req["process"]["cmd"].as_str().unwrap_or("").to_string();
        let starts = m
            .calls()
            .iter()
            .filter(|(n, c)| n == "Start" && c["process"]["cmd"] == cmd.as_str())
            .count();
        if cmd == "flaky" || (cmd == "flaky-gone" && starts == 1) {
            // The stream drops before the start event: did the process start?
            return;
        }
        if cmd == "badinput" {
            tx.send(frame(json!({"event": {"start": {"pid": 66}}})))
                .await
                .unwrap();
            m.wait_for(&["SendSignal"]).await;
            return;
        }
        if req.get("pty").is_some() {
            tx.send(frame(json!({"event": {"start": {"pid": 42}}})))
                .await
                .unwrap();
            tx.send(frame(data("pty", b"hello"))).await.unwrap();
            tx.send(frame(json!({"event": {"keepalive": {}}})))
                .await
                .unwrap();
            m.wait_for(&["SendInput", "Update"]).await;
            let input = m.find("SendInput").unwrap();
            let typed = unb64(input["input"]["pty"].as_str().unwrap());
            tx.send(frame(data("pty", &typed))).await.unwrap();
            tx.send(frame(
                json!({"event": {"end": {"exitCode": 3, "exited": true, "status": "exit status 3"}}}),
            ))
            .await
            .unwrap();
        } else if cmd == "chmod" {
            tx.send(frame(json!({"event": {"start": {"pid": 5}}})))
                .await
                .unwrap();
            // Protobuf JSON leaves out a zero exit code.
            tx.send(frame(
                json!({"event": {"end": {"exited": true, "status": "exit status 0"}}}),
            ))
            .await
            .unwrap();
        } else {
            tx.send(frame(json!({"event": {"start": {"pid": 8}}})))
                .await
                .unwrap();
            tx.send(frame(data("stdout", b"out"))).await.unwrap();
            tx.send(frame(data("stderr", b"err"))).await.unwrap();
            m.wait_for(&["SendInput", "CloseStdin"]).await;
            let input = m.find("SendInput").unwrap();
            let got = unb64(input["input"]["stdin"].as_str().unwrap());
            tx.send(frame(data("stdout", &got))).await.unwrap();
            tx.send(frame(
                json!({"event": {"end": {"exitCode": 7, "exited": true}}}),
            ))
            .await
            .unwrap();
        }
        tx.send(Ok(Bytes::from(envelope(FLAG_END_STREAM, b"{}"))))
            .await
            .unwrap();
    });
    Response::builder()
        .header("content-type", "application/connect+json")
        .body(Body::from_stream(rx))
        .unwrap()
}

async fn files(
    State(m): M,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if header(&h, "x-access-token") != ENVD_TOKEN {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code": 401, "message": "bad token"})),
        )
            .into_response();
    }
    m.record(
        "files",
        json!({
            "path": q.get("path"), "username": q.get("username"),
            "content_type": header(&h, "content-type"),
            "body": String::from_utf8_lossy(&body),
        }),
    );
    Json(json!([{"name": "b.sh", "path": q.get("path"), "type": "file"}])).into_response()
}

async fn serve() -> (E2b, Arc<Mock>) {
    let m = Arc::new(Mock::default());
    let app = Router::new()
        .route("/v2/sandboxes", get(list).post(create))
        .route("/sandboxes/{id}", get(get_one).delete(delete_one))
        .route("/sandboxes/{id}/pause", post(pause))
        .route("/sandboxes/{id}/snapshots", post(snapshot))
        .route("/v2/sandboxes/{id}/connect", post(connect))
        .route("/process.Process/{method}", post(rpc))
        .route("/files", post(files))
        .with_state(m.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    let mut p = E2b::new(ProviderConfig {
        api_url: Some(format!("http://{addr}")),
        ..Default::default()
    });
    p.envd_url = Some(format!("http://{addr}"));
    (p, m)
}

async fn recv(s: &mut Session) -> Out {
    tokio::time::timeout(Duration::from_secs(10), s.output.recv())
        .await
        .expect("timed out")
        .expect("output closed")
}

fn tok(s: &str) -> Secret {
    Secret::new(s)
}

fn spec() -> CreateSpec {
    let tags = naming::Tags {
        host: HOST.into(),
        key: BOXKEY.into(),
    };
    CreateSpec {
        name: naming::box_name(&tags),
        tags,
        env: [("A".to_string(), "1".to_string())].into_iter().collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn verify_maps_errors() {
    let (p, _) = serve().await;
    assert_eq!(p.verify(&tok(KEY)).await.unwrap().label, "E2B");
    let e = p.verify(&tok("e2b_bad")).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NeedsAuth);
    let e = p.verify(&tok(NOCARD)).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Account);
    assert!(e.message.contains("credit card"), "{}", e.message);
    assert!(!e.message.contains(NOCARD));
    assert_eq!(
        p.verify(&tok("")).await.unwrap_err().kind,
        ErrorKind::NeedsAuth
    );
}

#[tokio::test]
async fn list_pages_and_keeps_only_vibeke_boxes() {
    let (p, _) = serve().await;
    let boxes = p.list(&tok(KEY)).await.unwrap();
    let ids: Vec<_> = boxes.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, ["sbx1", "sbx3"]);
    assert_eq!(boxes[0].name, format!("vk-{HOST}-{BOXKEY}"));
    assert_eq!(boxes[0].state, BoxState::Running);
    assert_eq!(boxes[0].tags.as_ref().unwrap().key, BOXKEY);
    assert_eq!(boxes[0].created_at, 1_767_323_045);
    assert_eq!(boxes[1].state, BoxState::Paused);
}

#[tokio::test]
async fn create_get_and_lifecycle() {
    let (p, m) = serve().await;
    let b = p.create(&tok(KEY), &spec()).await.unwrap();
    assert_eq!(b.id, "sbx1");
    assert_eq!(b.state, BoxState::Running);
    assert_eq!(b.tags, Some(spec().tags));
    let body = m.find("create").unwrap();
    assert_eq!(body["templateID"], "base");
    assert_eq!(body["timeout"], 3600);
    assert_eq!(body["autoPause"], true);
    assert_eq!(body["metadata"]["vibeke_host"], HOST);
    assert_eq!(body["metadata"]["vibeke_key"], BOXKEY);
    assert_eq!(body["metadata"]["vibeke_name"], spec().name);
    assert_eq!(body["envVars"]["A"], "1");

    let g = p.get(&tok(KEY), "sbx1").await.unwrap();
    assert_eq!(g.name, spec().name);
    assert_eq!(g.state, BoxState::Running);
    assert_eq!(
        p.get(&tok("e2b_bad"), "sbx1").await.unwrap_err().kind,
        ErrorKind::NeedsAuth
    );
    assert_eq!(
        p.port_url(&tok(KEY), "sbx1", 3000)
            .await
            .unwrap()
            .as_deref(),
        Some("https://3000-sbx1.e2b.test")
    );
    // Already paused (409) and already gone (404) are success.
    p.suspend(&tok(KEY), "sbx1").await.unwrap();
    p.resume(&tok(KEY), "sbx1").await.unwrap();
    assert_eq!(m.find("connect").unwrap()["timeout"], 3600);
    assert_eq!(
        p.checkpoint(&tok(KEY), "sbx1", "before refactor")
            .await
            .unwrap(),
        "snap1:default"
    );
    p.destroy(&tok(KEY), "sbx1").await.unwrap();
}

#[tokio::test]
async fn tty_exec_streams_and_sends_input() {
    let (p, m) = serve().await;
    p.create(&tok(KEY), &spec()).await.unwrap();
    let mut s = p
        .exec(
            &tok(KEY),
            "sbx1",
            ExecReq {
                argv: vec!["bash".into(), "-l".into()],
                env: vec![("TERM".into(), "xterm-256color".into())],
                cwd: Some("/home/user".into()),
                tty: true,
                cols: 80,
                rows: 24,
                detachable: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(s.id, "42");
    assert!(s.tty);
    let start = m.find("Start").unwrap();
    assert_eq!(start["process"]["cmd"], "bash");
    assert_eq!(start["process"]["args"], json!(["-l"]));
    assert_eq!(start["process"]["envs"]["TERM"], "xterm-256color");
    assert_eq!(start["process"]["cwd"], "/home/user");
    assert_eq!(start["pty"]["size"], json!({"cols": 80, "rows": 24}));
    assert!(start["tag"].as_str().unwrap().starts_with("vk-tty-"));
    assert!(start.get("stdin").is_none());
    // The token from create was used: no connect call.
    assert!(m.find("connect").is_none());

    assert_eq!(recv(&mut s).await, Out::Stdout(b"hello".to_vec()));
    s.input.send(In::Data(b"ls\r".to_vec())).await.unwrap();
    s.input
        .send(In::Resize {
            cols: 120,
            rows: 40,
        })
        .await
        .unwrap();
    assert_eq!(recv(&mut s).await, Out::Stdout(b"ls\r".to_vec()));
    assert_eq!(recv(&mut s).await, Out::Exit(3));
    assert_eq!(
        m.find("SendInput").unwrap(),
        json!({"process": {"pid": 42}, "input": {"pty": "bHMN"}})
    );
    assert_eq!(
        m.find("Update").unwrap(),
        json!({"process": {"pid": 42}, "pty": {"size": {"cols": 120, "rows": 40}}})
    );
}

#[tokio::test]
async fn pipe_exec_refreshes_a_stale_token() {
    let (p, m) = serve().await;
    p.access.lock().unwrap().insert(
        "sbx1".into(),
        Access {
            token: Some("stale".into()),
            domain: "e2b.test".into(),
        },
    );
    let mut s = p
        .exec(
            &tok(KEY),
            "sbx1",
            ExecReq {
                argv: vec!["sh".into(), "-c".into(), "cat".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(m.find("connect").is_some());
    assert_eq!(s.id, "8");
    assert!(!s.tty);
    let start = m.find("Start").unwrap();
    assert_eq!(start["stdin"], true);
    assert!(start.get("pty").is_none());
    assert!(start["tag"].as_str().unwrap().starts_with("vk-pipe-"));
    assert_eq!(recv(&mut s).await, Out::Stdout(b"out".to_vec()));
    assert_eq!(recv(&mut s).await, Out::Stderr(b"err".to_vec()));
    s.input.send(In::Data(b"abc".to_vec())).await.unwrap();
    s.input.send(In::Eof).await.unwrap();
    assert_eq!(recv(&mut s).await, Out::Stdout(b"abc".to_vec()));
    assert_eq!(recv(&mut s).await, Out::Exit(7));
    assert_eq!(
        m.find("SendInput").unwrap()["input"],
        json!({"stdin": "YWJj"})
    );
    assert_eq!(
        m.find("CloseStdin").unwrap(),
        json!({"process": {"pid": 8}})
    );
}

#[tokio::test]
async fn sessions_and_attach() {
    let (p, m) = serve().await;
    let ss = p.sessions(&tok(KEY), "sbx1").await.unwrap();
    assert_eq!(ss.len(), 3);
    assert_eq!(ss[0].id, "42");
    assert_eq!(ss[0].command, "bash -l");
    assert!(ss[0].tty && ss[0].active);
    assert!(!ss[1].tty);
    assert_eq!(ss[1].command, "git status");

    let mut s = p.attach(&tok(KEY), "sbx1", "42", 100, 30).await.unwrap();
    assert_eq!(s.id, "42");
    assert!(s.tty);
    assert_eq!(m.find("Connect").unwrap(), json!({"process": {"pid": 42}}));
    assert_eq!(
        m.find("Update").unwrap()["pty"]["size"],
        json!({"cols": 100, "rows": 30})
    );
    assert_eq!(recv(&mut s).await, Out::Stdout(b"replay".to_vec()));
    assert!(matches!(recv(&mut s).await, Out::Lost(_)));

    assert_eq!(
        p.attach(&tok(KEY), "sbx1", "1234", 80, 24)
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::NotFound
    );
}

#[tokio::test]
async fn write_file_uploads_then_chmods() {
    let (p, m) = serve().await;
    p.write_file(
        &tok(KEY),
        "sbx1",
        "/home/user/a/b.sh",
        b"echo hi\n".to_vec(),
        0o755,
    )
    .await
    .unwrap();
    let f = m.find("files").unwrap();
    assert_eq!(f["path"], "/home/user/a/b.sh");
    assert_eq!(f["username"], "user");
    assert!(
        f["content_type"]
            .as_str()
            .unwrap()
            .starts_with("multipart/form-data; boundary=")
    );
    let body = f["body"].as_str().unwrap();
    assert!(body.contains("name=\"file\""), "{body}");
    assert!(body.contains("echo hi\n"), "{body}");
    let start = m.find("Start").unwrap();
    assert_eq!(start["process"]["cmd"], "chmod");
    assert_eq!(
        start["process"]["args"],
        json!(["755", "/home/user/a/b.sh"])
    );

    // The default mode needs no chmod.
    let (p, m) = serve().await;
    p.write_file(&tok(KEY), "sbx1", "/tmp/x", b"x".to_vec(), 0o644)
        .await
        .unwrap();
    assert!(m.find("files").is_some());
    assert!(m.find("Start").is_none());
}

#[test]
fn envelopes_round_trip() {
    let a = envelope(0, br#"{"a":1}"#);
    assert_eq!(&a[..5], &[0, 0, 0, 0, 7]);
    let b = envelope(FLAG_END_STREAM, b"{}");
    let mut f = Frames::default();
    let mut all = a.clone();
    all.extend_from_slice(&b);
    // Bytes arrive in pieces.
    f.push(&all[..3]);
    assert_eq!(f.next_frame().unwrap(), None);
    f.push(&all[3..9]);
    assert_eq!(f.next_frame().unwrap(), None);
    f.push(&all[9..]);
    assert_eq!(f.next_frame().unwrap(), Some((0, br#"{"a":1}"#.to_vec())));
    assert_eq!(
        f.next_frame().unwrap(),
        Some((FLAG_END_STREAM, b"{}".to_vec()))
    );
    assert_eq!(f.next_frame().unwrap(), None);

    let mut f = Frames::default();
    f.push(&[FLAG_COMPRESSED, 0, 0, 0, 0]);
    assert!(f.next_frame().is_err());
    let mut f = Frames::default();
    f.push(&[0, 0xff, 0xff, 0xff, 0xff]);
    assert!(f.next_frame().is_err());
}

#[test]
fn events_parse() {
    assert_eq!(
        parse_event(&json!({"event": {"start": {"pid": 12}}})),
        Event::Start(12)
    );
    assert_eq!(
        parse_event(&json!({"event": {"data": {"pty": "aGk="}}})),
        Event::Stdout(b"hi".to_vec())
    );
    assert_eq!(
        parse_event(&json!({"event": {"data": {"stdout": "aGk"}}})),
        Event::Stdout(b"hi".to_vec())
    );
    assert_eq!(
        parse_event(&json!({"event": {"data": {"stderr": "aGk="}}})),
        Event::Stderr(b"hi".to_vec())
    );
    assert_eq!(
        parse_event(&json!({"event": {"end": {"exitCode": 2, "exited": true}}})),
        Event::End(2)
    );
    assert_eq!(
        parse_event(&json!({"event": {"end": {"exited": true, "status": "exit status 0"}}})),
        Event::End(0)
    );
    assert_eq!(
        parse_event(&json!({"event": {"end": {"status": "signal: killed"}}})),
        Event::End(137)
    );
    // envd reports a signal death as exitCode -1 with a `signal: …` status.
    assert_eq!(
        parse_event(&json!({"event": {"end": {
            "exitCode": -1, "exited": false, "status": "signal: terminated"
        }}})),
        Event::End(143)
    );
    assert_eq!(
        end_code(&json!({"exitCode": -1, "status": "signal: killed"})),
        137
    );
    assert_eq!(
        end_code(&json!({"exitCode": -1, "status": "signal: segmentation fault (core dumped)"})),
        139
    );
    assert_eq!(end_code(&json!({"exitCode": -1, "status": "odd"})), 255);
    assert_eq!(
        parse_event(&json!({"event": {"keepalive": {}}})),
        Event::Other
    );
    let e = end_stream_error(
        &json!({"error": {"code": "not_found", "message": "process with pid 3 not found"}}),
    )
    .unwrap();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(end_stream_error(&json!({})).is_none());
}

#[test]
fn input_requests() {
    assert_eq!(
        input_request(3, true, &In::Data(b"hi".to_vec())),
        Some((
            "SendInput",
            json!({"process": {"pid": 3}, "input": {"pty": "aGk="}})
        ))
    );
    assert_eq!(
        input_request(3, false, &In::Data(b"hi".to_vec()))
            .unwrap()
            .1["input"],
        json!({"stdin": "aGk="})
    );
    assert_eq!(input_request(3, false, &In::Data(vec![])), None);
    assert_eq!(input_request(3, true, &In::Eof), None);
    assert_eq!(
        input_request(3, false, &In::Eof),
        Some(("CloseStdin", json!({"process": {"pid": 3}})))
    );
    assert_eq!(
        input_request(3, false, &In::Resize { cols: 1, rows: 1 }),
        None
    );
    assert_eq!(
        input_request(3, true, &In::Signal("sigkill".into()))
            .unwrap()
            .1["signal"],
        "SIGNAL_SIGKILL"
    );
    assert_eq!(
        input_request(3, true, &In::Signal("INT".into())).unwrap().1["signal"],
        "SIGNAL_SIGTERM"
    );
}

#[test]
fn error_mapping() {
    assert_eq!(http_error(401, b"{}").kind, ErrorKind::NeedsAuth);
    assert_eq!(
        http_error(403, br#"{"code":403,"message":"forbidden"}"#).kind,
        ErrorKind::NeedsAuth
    );
    assert_eq!(http_error(404, b"").kind, ErrorKind::NotFound);
    assert_eq!(
        http_error(429, br#"{"code":429,"message":"Rate limit exceeded"}"#).kind,
        ErrorKind::RateLimited
    );
    let e = http_error(
        429,
        br#"{"code":429,"message":"You have reached the maximum number of concurrent E2B sandboxes"}"#,
    );
    assert_eq!(e.kind, ErrorKind::Account);
    assert!(e.message.contains("concurrent"));
    assert_eq!(http_error(502, b"bad gateway").kind, ErrorKind::Unavailable);
    assert_eq!(
        http_error(400, br#"{"code":400,"message":"bad template"}"#).kind,
        ErrorKind::InvalidParams
    );
    assert_eq!(
        envd_http_error(502, b"sandbox not found").kind,
        ErrorKind::Unavailable
    );
    assert_eq!(
        envd_http_error(401, br#"{"code":"unauthenticated","message":"x"}"#).kind,
        ErrorKind::NeedsAuth
    );
    assert_eq!(
        rpc_error("resource_exhausted", "").kind,
        ErrorKind::RateLimited
    );
}

#[test]
fn boxes_and_cli_config_parse() {
    let b = parse_box(&listed("s", "paused", vk_meta(BOXKEY))).unwrap();
    assert_eq!(b.state, BoxState::Paused);
    assert_eq!(b.tags.unwrap().host, HOST);
    let b = parse_box(&listed("s", "running", json!({"vibeke_host": HOST}))).unwrap();
    assert_eq!(b.tags, None);
    assert_eq!(b.name, "s");
    assert_eq!(
        e2b_cli_key(r#"{"version":2,"projectApiKey":"e2b_abc","projectName":"x"}"#).as_deref(),
        Some("e2b_abc")
    );
    assert_eq!(
        e2b_cli_key(r#"{"version":1,"teamApiKey":"e2b_old"}"#).as_deref(),
        Some("e2b_old")
    );
    assert_eq!(e2b_cli_key(r#"{"tokens":{}}"#), None);
    assert_eq!(e2b_cli_key("not json"), None);
}

fn cache_token(p: &E2b, id: &str) {
    p.access.lock().unwrap().insert(
        id.into(),
        Access {
            token: Some(ENVD_TOKEN.into()),
            domain: "e2b.test".into(),
        },
    );
}

fn starts(m: &Mock) -> usize {
    m.calls().iter().filter(|(n, _)| n == "Start").count()
}

fn pipe(cmd: &str) -> ExecReq {
    ExecReq {
        argv: vec![cmd.into()],
        ..Default::default()
    }
}

#[tokio::test]
async fn an_ambiguous_start_attaches_to_the_started_process() {
    let (p, m) = serve().await;
    cache_token(&p, "sbx1");
    let mut s = p.exec(&tok(KEY), "sbx1", pipe("flaky")).await.unwrap();
    assert_eq!(s.id, "99");
    assert_eq!(starts(&m), 1, "Start must not run twice");
    assert!(m.find("List").is_some());
    assert_eq!(m.find("Connect").unwrap(), json!({"process": {"pid": 99}}));
    assert_eq!(recv(&mut s).await, Out::Stdout(b"replay".to_vec()));
}

#[tokio::test]
async fn an_ambiguous_start_runs_again_only_when_the_process_is_absent() {
    let (p, m) = serve().await;
    cache_token(&p, "sbx1");
    let s = p.exec(&tok(KEY), "sbx1", pipe("flaky-gone")).await.unwrap();
    assert_eq!(s.id, "8");
    assert_eq!(starts(&m), 2);
    assert!(m.find("List").is_some());
    assert!(m.find("Connect").is_none());

    // With freshly fetched access there is nothing to refresh: the error is returned.
    let (p, m) = serve().await;
    let e = p
        .exec(&tok(KEY), "sbx1", pipe("flaky"))
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Unavailable);
    assert_eq!(starts(&m), 1);
}

#[tokio::test]
async fn a_failed_input_fails_the_session_without_closing_stdin() {
    let (p, m) = serve().await;
    cache_token(&p, "sbx1");
    let mut s = p.exec(&tok(KEY), "sbx1", pipe("badinput")).await.unwrap();
    s.input.send(In::Data(b"part".to_vec())).await.unwrap();
    // The bridge may already have ended on the failed input.
    let _ = s.input.send(In::Eof).await;
    assert!(matches!(recv(&mut s).await, Out::Lost(_)));
    // The pipe session is not detachable: it is killed, never told its input ended.
    m.wait_for(&["SendSignal"]).await;
    assert!(m.find("CloseStdin").is_none());
}

#[tokio::test]
async fn reattach_does_not_wake_a_paused_sandbox() {
    let (p, m) = serve().await;
    let e = p.sessions(&tok(KEY), "paused1").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unavailable);
    assert!(e.message.contains("paused"), "{e}");
    let e = p
        .attach(&tok(KEY), "paused1", "42", 80, 24)
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::Unavailable);
    assert!(m.find("connect").is_none());
    // An explicit resume wakes it.
    p.resume(&tok(KEY), "paused1").await.unwrap();
    assert!(m.find("connect").is_some());
}
