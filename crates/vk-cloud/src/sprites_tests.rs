use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, put};
use serde_json::{Value, json};

use super::*;

const TOKEN: &str = "org/tok1/secret";
const CARD: &str = "org/tok2/nocard";
const VK: &str = "vk-0123abcd-0123456789";

fn auth(h: &HeaderMap) -> std::result::Result<(), (StatusCode, Json<Value>)> {
    let v = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if v == format!("Bearer {CARD}") {
        return Err((
            StatusCode::FORBIDDEN,
            Json(
                json!({"error": "Add a credit card to start using Sprites. Visit sprites.dev/account"}),
            ),
        ));
    }
    if v != format!("Bearer {TOKEN}") {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        ));
    }
    Ok(())
}

fn sprite(name: &str, status: &str) -> Value {
    json!({
        "id": format!("id-{name}"), "name": name, "organization": "acme", "status": status,
        "url": format!("https://{name}.sprites.app"),
        "created_at": "2026-01-02T03:04:05Z", "last_running_at": "2026-01-02T04:00:00.123Z",
        "last_warming_at": null, "url_settings": {"auth": "sprite"}
    })
}

async fn list(h: HeaderMap, RawQuery(q): RawQuery) -> (StatusCode, Json<Value>) {
    if let Err(e) = auth(&h) {
        return e;
    }
    let q = q.unwrap_or_default();
    let org = json!({"name": "Acme", "running_limit": 10, "warm_limit": 20});
    if q.contains("continuation_token=p2") {
        return (
            StatusCode::OK,
            Json(
                json!({"sprites": [sprite("vk-0123abcd-aaaaaaaaaa", "running")], "has_more": false, "org": org}),
            ),
        );
    }
    assert!(
        q.contains("prefix=vk-") || q.contains("max_results=1"),
        "{q}"
    );
    (
        StatusCode::OK,
        Json(json!({
            "sprites": [sprite(VK, "warm"), sprite("my-own-sprite", "running")],
            "has_more": true, "next_continuation_token": "p2", "org": org
        })),
    )
}

async fn create(h: HeaderMap, body: String) -> (StatusCode, Json<Value>) {
    if let Err(e) = auth(&h) {
        return e;
    }
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["url_settings"]["auth"], "sprite");
    let name = v["name"].as_str().unwrap();
    if name.ends_with("dddddddddd") {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "sprite already exists"})),
        );
    }
    (StatusCode::CREATED, Json(sprite(name, "cold")))
}

async fn delete_one(h: HeaderMap, Path(_name): Path<String>) -> (StatusCode, Json<Value>) {
    if let Err(e) = auth(&h) {
        return e;
    }
    (StatusCode::NOT_FOUND, Json(json!({"error": "not found"})))
}

async fn get_one(h: HeaderMap, Path(name): Path<String>) -> (StatusCode, Json<Value>) {
    if let Err(e) = auth(&h) {
        return e;
    }
    (StatusCode::OK, Json(sprite(&name, "running")))
}

async fn exec(
    ws: WebSocketUpgrade,
    h: HeaderMap,
    RawQuery(q): RawQuery,
) -> std::result::Result<Response, (StatusCode, Json<Value>)> {
    auth(&h)?;
    let q = q.unwrap_or_default();
    Ok(ws.on_upgrade(move |s| async move {
        if q.contains("tty=true") {
            tty_session(s, q).await
        } else {
            pipe_session(s).await
        }
    }))
}

async fn next_msg(s: &mut WebSocket) -> AxMsg {
    loop {
        match s.recv().await.expect("client closed").expect("ws error") {
            AxMsg::Ping(_) | AxMsg::Pong(_) => continue,
            m => return m,
        }
    }
}

async fn tty_session(mut s: WebSocket, q: String) {
    s.send(AxMsg::text(
        json!({"type": "session_info", "session_id": "s1", "tty": true}).to_string(),
    ))
    .await
    .unwrap();
    s.send(AxMsg::binary(format!("q:{q}").into_bytes()))
        .await
        .unwrap();
    // A resize comes as JSON text.
    let AxMsg::Text(t) = next_msg(&mut s).await else {
        panic!("expected resize text");
    };
    let v: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(v["type"], "resize");
    s.send(AxMsg::binary(
        format!("size {}x{}", v["cols"], v["rows"]).into_bytes(),
    ))
    .await
    .unwrap();
    // Raw input comes as binary, unprefixed.
    let AxMsg::Binary(b) = next_msg(&mut s).await else {
        panic!("expected binary input");
    };
    assert_eq!(&b[..], b"ls\r");
    s.send(AxMsg::text(
        json!({"type": "port_opened", "port": 8080, "address": "https://x.sprites.app", "pid": 9})
            .to_string(),
    ))
    .await
    .unwrap();
    s.send(AxMsg::text(
        json!({"type": "exit", "exit_code": 3}).to_string(),
    ))
    .await
    .unwrap();
    let _ = s.recv().await;
}

async fn pipe_session(mut s: WebSocket) {
    s.send(AxMsg::binary(b"\x01out".to_vec())).await.unwrap();
    s.send(AxMsg::binary(b"\x02err".to_vec())).await.unwrap();
    let mut got = Vec::new();
    loop {
        let AxMsg::Binary(b) = next_msg(&mut s).await else {
            panic!("expected binary");
        };
        match b[0] {
            0x00 => got.extend_from_slice(&b[1..]),
            0x04 => break,
            x => panic!("unexpected stream byte {x}"),
        }
    }
    let mut echo = vec![0x01];
    echo.extend_from_slice(&got);
    s.send(AxMsg::binary(echo)).await.unwrap();
    s.send(AxMsg::binary(vec![0x03, 7])).await.unwrap();
    let _ = s.recv().await;
}

async fn attach(
    ws: WebSocketUpgrade,
    h: HeaderMap,
    Path((_name, sid)): Path<(String, String)>,
) -> std::result::Result<Response, (StatusCode, Json<Value>)> {
    auth(&h)?;
    Ok(ws.on_upgrade(move |mut s| async move {
        s.send(AxMsg::text(
            json!({"type": "session_info", "session_id": sid, "command": "bash", "cols": 80, "rows": 24, "tty": true, "is_owner": true}).to_string(),
        ))
        .await
        .unwrap();
        // The attach resize arrives first.
        let AxMsg::Text(t) = next_msg(&mut s).await else {
            panic!("expected resize");
        };
        assert!(t.as_str().contains("\"cols\":100"));
        s.send(AxMsg::binary(b"replay".to_vec())).await.unwrap();
        // Then the socket drops without an exit: the client sees Lost.
    }))
}

async fn write(
    h: HeaderMap,
    Path(name): Path<String>,
    RawQuery(q): RawQuery,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = auth(&h) {
        return e;
    }
    let q = q.unwrap_or_default();
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(q.as_bytes())
        .into_owned()
        .collect();
    pairs.sort();
    let want = [
        ("mkdirParents".to_string(), "true".to_string()),
        ("mode".to_string(), "0755".to_string()),
        ("path".to_string(), "/usr/local/bin/vibeke".to_string()),
    ];
    if name != VK || pairs != want || &body[..] != b"ELF" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("unexpected write: {name} {q}")})),
        );
    }
    (StatusCode::OK, Json(json!({})))
}

async fn serve() -> Sprites {
    let app = Router::new()
        .route("/v1/sprites", get(list).post(create))
        .route("/v1/sprites/{name}", get(get_one).delete(delete_one))
        .route("/v1/sprites/{name}/exec", get(exec))
        .route("/v1/sprites/{name}/exec/{sid}", get(attach))
        .route("/v1/sprites/{name}/fs/write", put(write));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    Sprites::new(ProviderConfig {
        api_url: Some(format!("http://{addr}/v1")),
        ..Default::default()
    })
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

#[tokio::test]
async fn verify_labels_the_org_and_maps_errors() {
    let p = serve().await;
    let a = p.verify(&tok(TOKEN)).await.unwrap();
    assert_eq!(a.label, "Acme");
    assert_eq!(
        a.details.get("running_limit").map(String::as_str),
        Some("10")
    );
    let e = p.verify(&tok("org/x/bad")).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NeedsAuth);
    let e = p.verify(&tok(CARD)).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Account);
    assert!(e.message.contains("credit card"), "{}", e.message);
    assert!(!e.message.contains("nocard"));
}

#[tokio::test]
async fn list_pages_and_keeps_only_vibeke_boxes() {
    let p = serve().await;
    let boxes = p.list(&tok(TOKEN)).await.unwrap();
    let names: Vec<_> = boxes.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, [VK, "vk-0123abcd-aaaaaaaaaa"]);
    assert_eq!(boxes[0].state, BoxState::Warm);
    assert_eq!(boxes[0].tags.as_ref().unwrap().host, "0123abcd");
    assert_eq!(boxes[0].created_at, 1_767_323_045);
    assert_eq!(boxes[0].last_active_at, 1_767_326_400);
    assert_eq!(boxes[1].state, BoxState::Running);
}

#[tokio::test]
async fn create_get_destroy() {
    let p = serve().await;
    let tags = naming::parse_name(VK).unwrap();
    let spec = CreateSpec {
        name: VK.into(),
        tags: tags.clone(),
        ..Default::default()
    };
    let b = p.create(&tok(TOKEN), &spec).await.unwrap();
    assert_eq!(b.id, VK);
    assert_eq!(b.state, BoxState::Cold);
    assert_eq!(b.tags, Some(tags));
    let dup = CreateSpec {
        name: "vk-0123abcd-dddddddddd".into(),
        ..Default::default()
    };
    assert_eq!(
        p.create(&tok(TOKEN), &dup).await.unwrap_err().kind,
        ErrorKind::Conflict
    );
    // 404 on delete is success.
    p.destroy(&tok(TOKEN), VK).await.unwrap();
    assert_eq!(
        p.port_url(&tok(TOKEN), VK, 8080).await.unwrap().as_deref(),
        Some("https://vk-0123abcd-0123456789.sprites.app")
    );
    assert_eq!(p.port_url(&tok(TOKEN), VK, 3000).await.unwrap(), None);
    assert_eq!(
        p.get(&tok("bad/bad/bad"), VK).await.unwrap_err().kind,
        ErrorKind::NeedsAuth
    );
}

#[tokio::test]
async fn write_file_uses_the_sdk_parameter_names() {
    let p = serve().await;
    p.write_file(
        &tok(TOKEN),
        VK,
        "/usr/local/bin/vibeke",
        b"ELF".to_vec(),
        0o100755,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn tty_exec_frames() {
    let p = serve().await;
    let mut s = p
        .exec(
            &tok(TOKEN),
            VK,
            ExecReq {
                argv: vec!["bash".into(), "-l".into()],
                env: vec![("TERM".into(), "xterm-256color".into())],
                cwd: Some("/workspace".into()),
                tty: true,
                cols: 80,
                rows: 24,
                detachable: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(s.id, "s1");
    let Out::Stdout(q) = recv(&mut s).await else {
        panic!("expected the query echo");
    };
    let q = String::from_utf8(q).unwrap();
    for part in [
        "cmd=bash",
        "cmd=-l",
        "path=bash",
        "tty=true",
        "stdin=true",
        "cols=80",
        "rows=24",
        "dir=%2Fworkspace",
        "env=TERM%3Dxterm-256color",
        "detachable=true",
    ] {
        assert!(q.contains(part), "{part} missing from {q}");
    }
    s.input
        .send(In::Resize {
            cols: 120,
            rows: 40,
        })
        .await
        .unwrap();
    assert_eq!(recv(&mut s).await, Out::Stdout(b"size 120x40".to_vec()));
    s.input.send(In::Data(b"ls\r".to_vec())).await.unwrap();
    assert_eq!(
        recv(&mut s).await,
        Out::PortOpened {
            port: 8080,
            url: Some("https://x.sprites.app".into())
        }
    );
    assert_eq!(recv(&mut s).await, Out::Exit(3));
}

#[tokio::test]
async fn pipe_exec_frames() {
    let p = serve().await;
    let mut s = p
        .exec(
            &tok(TOKEN),
            VK,
            ExecReq {
                argv: vec!["git".into(), "upload-pack".into(), "/workspace".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!s.tty);
    assert_eq!(recv(&mut s).await, Out::Stdout(b"out".to_vec()));
    assert_eq!(recv(&mut s).await, Out::Stderr(b"err".to_vec()));
    s.input.send(In::Data(b"abc".to_vec())).await.unwrap();
    s.input.send(In::Eof).await.unwrap();
    assert_eq!(recv(&mut s).await, Out::Stdout(b"abc".to_vec()));
    assert_eq!(recv(&mut s).await, Out::Exit(7));
}

#[tokio::test]
async fn attach_replays_and_reports_loss() {
    let p = serve().await;
    let mut s = p.attach(&tok(TOKEN), VK, "s9", 100, 30).await.unwrap();
    assert_eq!(s.id, "s9");
    assert!(s.tty);
    assert_eq!(recv(&mut s).await, Out::Stdout(b"replay".to_vec()));
    assert!(matches!(recv(&mut s).await, Out::Lost(_)));
}

#[tokio::test]
async fn exec_with_a_rejected_token_needs_auth() {
    let p = serve().await;
    let e = p
        .exec(
            &tok("x/y/z"),
            VK,
            ExecReq {
                argv: vec!["true".into()],
                ..Default::default()
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind, ErrorKind::NeedsAuth);
}

#[test]
fn error_mapping() {
    assert_eq!(http_error(429, b"").kind, ErrorKind::RateLimited);
    assert_eq!(http_error(404, b"{}").kind, ErrorKind::NotFound);
    assert_eq!(http_error(503, b"oops").kind, ErrorKind::Unavailable);
    let e = http_error(
        403,
        br#"{"error":"Add a credit card to start using Sprites"}"#,
    );
    assert_eq!(e.kind, ErrorKind::Account);
    assert_eq!(e.message, "Add a credit card to start using Sprites");
    assert_eq!(
        http_error(403, br#"{"error":"forbidden"}"#).kind,
        ErrorKind::NeedsAuth
    );
}

#[test]
fn timestamps() {
    assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339("2026-01-02T03:04:05Z"), Some(1_767_323_045));
    assert_eq!(
        parse_rfc3339("2026-01-02T05:04:05.5+02:00"),
        Some(1_767_323_045)
    );
    assert_eq!(parse_rfc3339("nope"), None);
}

#[test]
fn sessions_and_checkpoints_parse() {
    let v = json!({"sessions": [
        {"id": 12, "command": "bash -l", "workdir": "/workspace", "tty": true, "is_active": true,
         "created": "2026-01-02T03:04:05Z", "last_activity": "2026-01-02T04:00:00Z"},
        {"id": "13", "command": ["git", "status"], "tty": false, "is_active": false}
    ]});
    let s = sessions_from(&v);
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].id, "12");
    assert!(s[0].active && s[0].tty);
    assert_eq!(s[0].last_activity_at, 1_767_326_400);
    assert_eq!(s[1].command, "git status");
    assert_eq!(sessions_from(&json!([{"id": "a"}]))[0].id, "a");
    assert_eq!(
        checkpoint_id(
            "{\"type\":\"info\",\"data\":\"x\"}\n{\"type\":\"complete\",\"id\":\"v3\"}\n"
        )
        .unwrap(),
        "v3"
    );
    assert_eq!(checkpoint_id("progress\n").unwrap(), "latest");
    assert!(checkpoint_id("{\"type\":\"error\",\"error\":\"disk\"}").is_err());
}

#[test]
fn import_helpers() {
    assert_eq!(
        pick_fly_org(r#"{"acme": "Acme Inc", "personal": "Jo Personal"}"#).as_deref(),
        Some("personal")
    );
    assert_eq!(
        pick_fly_org(r#"[{"slug":"acme","type":"SHARED"},{"slug":"jo","type":"PERSONAL"}]"#)
            .as_deref(),
        Some("jo")
    );
    assert_eq!(
        sprite_cli_token(r#"{"orgs":{"acme":{"api_token":"acme/123/abc","url":"https://x"}}}"#)
            .as_deref(),
        Some("acme/123/abc")
    );
    assert_eq!(
        sprite_cli_token(r#"{"orgs":{"acme":{"keyring":true}}}"#),
        None
    );
}
