//! Drives the read-only JSON API (`/api/tree`, `/api/nodes/{id}`) over real HTTP against
//! a server bound to a random port with a temp SQLite file. Data is created through MCP
//! (`POST /mcp`) or, for the big shapes, seeded by SQL before the server starts.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{HeaderMap, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use spokes::config::Config;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct Server {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

async fn start(db_path: &Path) -> Server {
    start_cfg(db_path, None, Vec::new()).await
}

async fn start_cfg(
    db_path: &Path,
    allowed_hosts: Option<Vec<String>>,
    allowed_origins: Vec<String>,
) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        bind: addr,
        db_path: db_path.to_path_buf(),
        allowed_hosts,
        allowed_origins,
        tz: chrono_tz::UTC,
        backup: None,
    };
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        spokes::serve(listener, &config, async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    });
    Server { addr, stop, task }
}

impl Server {
    async fn shutdown(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap();
    }
}

/// One HTTP response.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&self.body)))
    }

    fn etag(&self) -> String {
        self.headers
            .get("etag")
            .unwrap_or_else(|| panic!("no ETag on {}", self.status))
            .to_str()
            .unwrap()
            .to_string()
    }
}

struct Mcp {
    client: Client<HttpConnector, Full<Bytes>>,
    url: String,
    base: String,
}

impl Mcp {
    fn new(addr: SocketAddr) -> Self {
        Mcp {
            client: Client::builder(TokioExecutor::new()).build_http(),
            url: format!("http://{addr}/mcp"),
            base: format!("http://{addr}"),
        }
    }

    async fn post(&self, body: Value, headers: &[(&str, &str)]) -> (StatusCode, String) {
        let mut req = Request::post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = req.body(Full::new(Bytes::from(body.to_string()))).unwrap();
        let res = self.client.request(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        let (status, body) = self
            .post(
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
                &[("mcp-protocol-version", "2025-11-25")],
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{method}: {body}");
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{method}: {e}: {body}"))
    }

    /// Calls a tool; returns (text, is_error).
    async fn call(&self, name: &str, arguments: Value) -> (String, bool) {
        let v = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let result = &v["result"];
        assert!(result.is_object(), "{name} returned no result: {v}");
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        (text, result["isError"].as_bool().unwrap_or(false))
    }

    async fn ok(&self, name: &str, arguments: Value) -> String {
        let (text, is_error) = self.call(name, arguments).await;
        assert!(!is_error, "{name} failed: {text}");
        text
    }

    /// Any request with extra headers.
    async fn request(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Reply {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = self
            .client
            .request(req.body(Full::new(Bytes::new())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> Reply {
        self.request("GET", path, headers).await
    }

    /// GET expecting 200; the JSON body.
    async fn j(&self, path: &str) -> Value {
        let r = self.get(path, &[]).await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&r.body)
        );
        r.json()
    }

    /// Adds a node under `parent` (None = root); returns nothing, look ids up by title.
    async fn add(&self, parent: Option<i64>, spec: Value) {
        let mut spec = spec;
        if let Some(p) = parent {
            spec["parent"] = json!(format!("SPOKE-{p}"));
        }
        self.ok("add", spec).await;
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spokes-api-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn db(path: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn
}

fn ts_offset(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(seconds))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The id of the node with this (unique) title.
fn id_of(path: &Path, title: &str) -> i64 {
    db(path)
        .query_row("SELECT id FROM nodes WHERE title = ?1", [title], |r| {
            r.get(0)
        })
        .unwrap_or_else(|e| panic!("no node titled {title}: {e}"))
}

fn sid(id: i64) -> String {
    format!("SPOKE-{id}")
}

fn ids(v: &Value) -> Vec<i64> {
    v["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect()
}

fn node(v: &Value, id: i64) -> &Value {
    v["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"].as_i64() == Some(id))
        .unwrap_or_else(|| panic!("node {id} not in response"))
}

fn count(v: &Value, id: i64, key: &str) -> u64 {
    node(v, id)[key].as_u64().unwrap()
}

/// Creates the database (schema and root) and runs `f` in one transaction to seed rows.
fn seed(path: &Path, f: impl FnOnce(&rusqlite::Connection)) {
    let mut conn = spokes::db::open(path).unwrap();
    let tx = conn.transaction().unwrap();
    f(&tx);
    tx.commit().unwrap();
}

fn ins(tx: &rusqlite::Connection, parent: i64, pos: usize, kind: &str, title: &str) -> i64 {
    let ts = "2026-01-01T00:00:00Z";
    tx.prepare_cached(
        "INSERT INTO nodes (user_id, parent_id, position, kind, title, status,
                            created_at, updated_at, touched_at)
         VALUES (1, ?1, ?2, ?3, ?4, 'open', ?5, ?5, ?5)",
    )
    .unwrap()
    .execute(rusqlite::params![
        parent,
        format!("a{pos:06}"),
        kind,
        title,
        ts
    ])
    .unwrap();
    tx.last_insert_rowid()
}

/// The slice never holds half of a node's children (except the root's own may be cut).
fn check_whole_groups(v: &Value, root: i64) {
    let nodes = v["nodes"].as_array().unwrap();
    for n in nodes {
        let id = n["id"].as_i64().unwrap();
        let present = nodes
            .iter()
            .filter(|c| c["parent"].as_i64() == Some(id))
            .count() as u64;
        let child_count = n["child_count"].as_u64().unwrap();
        if n["children_loaded"].as_bool().unwrap() {
            assert_eq!(present, child_count, "node {id} claims loaded");
        } else if id != root {
            assert_eq!(
                present, 0,
                "node {id} not loaded but has {present} children"
            );
        } else {
            assert!(present < child_count);
        }
    }
}

#[tokio::test]
async fn slices_by_root_and_depth() {
    let dir = temp_dir("slices");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    m.add(
        None,
        json!({"title": "Home", "kind": "area", "children": [
            {"title": "Plan", "children": [
                {"title": "A"},
                {"title": "B", "children": [{"title": "B1"}, {"title": "B2"}]},
                {"title": "C"}
            ]}
        ]}),
    )
    .await;
    m.add(None, json!({"title": "Work", "kind": "area"})).await;
    let (home, plan, a, b, c, b1, b2, work) = (
        id_of(&path, "Home"),
        id_of(&path, "Plan"),
        id_of(&path, "A"),
        id_of(&path, "B"),
        id_of(&path, "C"),
        id_of(&path, "B1"),
        id_of(&path, "B2"),
        id_of(&path, "Work"),
    );
    m.ok("move", json!({"id": sid(c), "before": sid(a)})).await;

    let v = m.j("/api/tree").await;
    assert_eq!(v["root"], 1);
    assert_eq!(v["depth"], 3);
    assert_eq!(v["include_closed"], false);
    assert_eq!(v["node_limit"], 2000);
    assert_eq!(v["truncated"], false);
    assert_eq!(v["path"], json!([]));
    assert!(v["rev"].as_i64().unwrap() > 0);
    assert!(v["today"].as_str().unwrap().len() == 10);
    assert_eq!(v["time_zone"], "UTC");
    assert!(v["version"].is_string());
    // Level 3 is Plan's children; B's children (level 4) are not in the response.
    assert_eq!(ids(&v), vec![1, home, plan, c, a, b, work]);
    assert!(a < c, "C was added after A, so ids alone would put A first");
    assert!(v["nodes"][0]["parent"].is_null());
    assert_eq!(node(&v, home)["parent"], 1);
    assert_eq!(node(&v, plan)["parent"], home);
    assert_eq!(node(&v, b)["parent"], plan);

    let want = [
        (1, 2, 8, true),
        (home, 1, 6, true),
        (plan, 3, 5, true),
        (c, 0, 0, true),
        (a, 0, 0, true),
        (b, 2, 2, false),
        (work, 0, 0, true),
    ];
    for (id, kids, desc, loaded) in want {
        assert_eq!(count(&v, id, "child_count"), kids, "child_count {id}");
        assert_eq!(count(&v, id, "descendant_count"), desc, "descendants {id}");
        assert_eq!(node(&v, id)["children_loaded"], loaded, "loaded {id}");
    }
    assert_eq!(node(&v, home)["kind"], "area");
    assert_eq!(node(&v, plan)["kind"], "task");
    assert_eq!(node(&v, plan)["title"], "Plan");

    // Depth 0 is the root alone; its children are counted but not loaded.
    let v = m.j("/api/tree?depth=0").await;
    assert_eq!(ids(&v), vec![1]);
    assert_eq!(v["depth"], 0);
    assert_eq!(count(&v, 1, "child_count"), 2);
    assert_eq!(node(&v, 1)["children_loaded"], false);

    // Depth 1 from the root.
    let v = m.j("/api/tree?depth=1").await;
    assert_eq!(ids(&v), vec![1, home, work]);
    assert_eq!(node(&v, home)["children_loaded"], false);
    assert_eq!(node(&v, work)["children_loaded"], true);

    // Both id spellings; breadcrumb is root-first and includes SPOKE-1.
    for spelling in [sid(plan), plan.to_string()] {
        let v = m.j(&format!("/api/tree?root={spelling}&depth=2")).await;
        assert_eq!(v["root"], plan);
        assert_eq!(ids(&v), vec![plan, c, a, b, b1, b2]);
        assert_eq!(
            v["path"],
            json!([{"id": 1, "title": "To do"}, {"id": home, "title": "Home"}])
        );
        assert_eq!(node(&v, plan)["parent"], home);
        assert_eq!(node(&v, b)["children_loaded"], true);
    }
    let v = m.j(&format!("/api/tree?root={plan}&depth=1")).await;
    assert_eq!(ids(&v), vec![plan, c, a, b]);
    assert_eq!(node(&v, b)["children_loaded"], false);
    let v = m.j(&format!("/api/tree?root={plan}&depth=0")).await;
    assert_eq!(ids(&v), vec![plan]);
    assert_eq!(node(&v, plan)["children_loaded"], false);
    assert_eq!(count(&v, plan, "descendant_count"), 5);

    // A leaf as root.
    let v = m.j(&format!("/api/tree?root={b2}")).await;
    assert_eq!(ids(&v), vec![b2]);
    assert_eq!(v["path"].as_array().unwrap().len(), 4);

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn closed_nodes_are_hidden_unless_asked_for() {
    let dir = temp_dir("closed");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    m.add(
        None,
        json!({"title": "Ar", "kind": "area", "children": [
            {"title": "Keep", "children": [{"title": "KeepKid"}]},
            {"title": "Fin", "children": [{"title": "FinKid"}, {"title": "FinKid2"}]},
            {"title": "Drop"}
        ]}),
    )
    .await;
    let (ar, keep, keepkid, fin, finkid, finkid2, drop) = (
        id_of(&path, "Ar"),
        id_of(&path, "Keep"),
        id_of(&path, "KeepKid"),
        id_of(&path, "Fin"),
        id_of(&path, "FinKid"),
        id_of(&path, "FinKid2"),
        id_of(&path, "Drop"),
    );
    m.ok(
        "set_status",
        json!({"ids": [sid(finkid), sid(finkid2)], "status": "done"}),
    )
    .await;
    m.ok("set_status", json!({"ids": [sid(fin)], "status": "done"}))
        .await;
    m.ok(
        "set_status",
        json!({"ids": [sid(drop)], "status": "dropped"}),
    )
    .await;

    let v = m.j(&format!("/api/tree?root={ar}&depth=5")).await;
    assert_eq!(ids(&v), vec![ar, keep, keepkid]);
    assert_eq!(count(&v, ar, "child_count"), 1);
    assert_eq!(count(&v, ar, "descendant_count"), 2);
    assert_eq!(count(&v, ar, "closed_child_count"), 2);
    assert_eq!(node(&v, ar)["children_loaded"], true);
    assert_eq!(count(&v, keep, "closed_child_count"), 0);

    let v = m
        .j(&format!("/api/tree?root={ar}&depth=5&include_closed=true"))
        .await;
    assert_eq!(v["include_closed"], true);
    assert_eq!(ids(&v), vec![ar, keep, keepkid, fin, finkid, finkid2, drop]);
    assert_eq!(count(&v, ar, "child_count"), 3);
    assert_eq!(count(&v, ar, "descendant_count"), 6);
    assert_eq!(count(&v, ar, "closed_child_count"), 2);
    assert_eq!(count(&v, fin, "closed_child_count"), 2);
    assert_eq!(node(&v, fin)["status"], "done");
    assert_eq!(node(&v, drop)["status"], "dropped");
    for flag in ["1", "true"] {
        let v = m
            .j(&format!(
                "/api/tree?root={ar}&depth=5&include_closed={flag}"
            ))
            .await;
        assert_eq!(ids(&v).len(), 7);
    }
    for flag in ["0", "false"] {
        let v = m
            .j(&format!(
                "/api/tree?root={ar}&depth=5&include_closed={flag}"
            ))
            .await;
        assert_eq!(ids(&v).len(), 3);
    }

    // A closed node asked for as root is always returned.
    let v = m.j(&format!("/api/tree?root={drop}")).await;
    assert_eq!(ids(&v), vec![drop]);
    assert_eq!(node(&v, drop)["status"], "dropped");
    let v = m.j(&format!("/api/tree?root={fin}&depth=0")).await;
    assert_eq!(ids(&v), vec![fin]);

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn node_fields_have_the_documented_values() {
    let dir = temp_dir("fields");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    let later = (chrono::Utc::now() + chrono::Duration::days(30))
        .format("%Y-%m-%d")
        .to_string();
    m.add(
        None,
        json!({"title": "Fields", "kind": "area", "children": [
            {"title": "Pri", "kind": "area", "priority": 2, "children": [
                {"title": "Mid", "children": [{"title": "Leaf"}]}]},
            {"title": "Foc2"},
            {"title": "Later", "due": "2030-01-01"},
            {"title": "Steps", "ordered": true, "children": [{"title": "S1"}, {"title": "S2"}]},
            {"title": "Claimed"},
            {"title": "Asked"},
            {"title": "Answered"},
            {"title": "Chore", "recur": "every 2 weeks"},
            {"title": "Noted", "notes": "some body"},
            {"title": "Bare"},
            {"title": "Prog", "children": [{"title": "x"}, {"title": "y"}, {"title": "z"}]}
        ]}),
    )
    .await;
    let g = |t: &str| id_of(&path, t);
    let root = g("Fields");

    m.ok(
        "update",
        json!({"id": sid(g("Later")), "defer_until": later}),
    )
    .await;
    m.ok("focus", json!({"id": sid(g("Leaf")), "rank": 1}))
        .await;
    m.ok("focus", json!({"id": sid(g("Foc2")), "rank": 2}))
        .await;
    m.ok(
        "claim",
        json!({"id": sid(g("Claimed")), "agent": "w1", "lease_minutes": 30}),
    )
    .await;
    m.ok("claim", json!({"id": sid(g("Asked")), "agent": "w2"}))
        .await;
    m.ok(
        "ask",
        json!({"id": sid(g("Asked")), "agent": "w2", "question": "Which disk?"}),
    )
    .await;
    m.ok("claim", json!({"id": sid(g("Answered")), "agent": "w3"}))
        .await;
    m.ok(
        "ask",
        json!({"id": sid(g("Answered")), "agent": "w3", "question": "Which port?"}),
    )
    .await;
    m.ok("answer", json!({"id": sid(g("Answered")), "text": "8080"}))
        .await;
    m.ok(
        "set_status",
        json!({"ids": [sid(g("x"))], "status": "done"}),
    )
    .await;
    m.ok(
        "set_status",
        json!({"ids": [sid(g("z"))], "status": "dropped"}),
    )
    .await;

    let v = m
        .j(&format!(
            "/api/tree?root={root}&depth=5&include_closed=true"
        ))
        .await;

    // Every field is present on every node.
    let keys = [
        "id",
        "parent",
        "kind",
        "ordered",
        "title",
        "status",
        "priority",
        "effective_priority",
        "focus",
        "due",
        "defer_until",
        "deferred",
        "claimed_by",
        "claim_expires",
        "stalled",
        "waiting_on",
        "answered",
        "repeat",
        "has_notes",
        "unplanned",
        "is_step",
        "is_next",
        "progress",
        "created_at",
        "updated_at",
        "touched_at",
        "child_count",
        "descendant_count",
        "closed_child_count",
        "children_loaded",
    ];
    for n in v["nodes"].as_array().unwrap() {
        for k in keys {
            assert!(n.get(k).is_some(), "node {} lacks {k}", n["id"]);
        }
    }

    // Priority inheritance.
    assert_eq!(node(&v, g("Pri"))["priority"], 2);
    assert_eq!(node(&v, g("Pri"))["effective_priority"], 2);
    assert!(node(&v, g("Leaf"))["priority"].is_null());
    assert_eq!(node(&v, g("Leaf"))["effective_priority"], 2);
    assert!(node(&v, g("Foc2"))["effective_priority"].is_null());

    // Focus rank.
    assert_eq!(node(&v, g("Leaf"))["focus"], 1);
    assert_eq!(node(&v, g("Foc2"))["focus"], 2);
    assert!(node(&v, g("Mid"))["focus"].is_null());

    // Due and defer.
    let later_node = node(&v, g("Later"));
    assert_eq!(later_node["due"], "2030-01-01");
    assert_eq!(later_node["defer_until"], later);
    assert_eq!(later_node["deferred"], true);
    assert_eq!(node(&v, g("Foc2"))["deferred"], false);

    // Ordered parent: steps and next.
    assert_eq!(node(&v, g("Steps"))["ordered"], true);
    assert_eq!(node(&v, g("S1"))["is_step"], true);
    assert_eq!(node(&v, g("S2"))["is_step"], true);
    assert_eq!(node(&v, g("S1"))["is_next"], true);
    assert_eq!(node(&v, g("S2"))["is_next"], false);
    assert_eq!(node(&v, g("Foc2"))["is_step"], false);
    assert_eq!(node(&v, g("Foc2"))["is_next"], false);
    m.ok(
        "set_status",
        json!({"ids": [sid(g("S1"))], "status": "done"}),
    )
    .await;
    let v2 = m
        .j(&format!(
            "/api/tree?root={root}&depth=5&include_closed=true"
        ))
        .await;
    assert_eq!(node(&v2, g("S1"))["is_next"], false);
    assert_eq!(node(&v2, g("S2"))["is_next"], true);
    assert_eq!(
        node(&v2, g("Steps"))["progress"],
        json!({"done": 1, "total": 2})
    );

    // Claim.
    let cl = node(&v, g("Claimed"));
    assert_eq!(cl["status"], "in_progress");
    assert_eq!(cl["claimed_by"], "w1");
    assert!(cl["claim_expires"].as_str().unwrap().ends_with('Z'));
    assert_eq!(cl["stalled"], false);
    assert!(node(&v, g("Foc2"))["claimed_by"].is_null());

    // Ask and answer.
    let asked = node(&v, g("Asked"));
    assert_eq!(asked["status"], "waiting");
    assert_eq!(asked["waiting_on"], "Which disk?");
    assert_eq!(asked["answered"], false);
    let ans = node(&v, g("Answered"));
    assert_eq!(ans["answered"], true);
    assert_eq!(ans["status"], "open");

    // Repeat.
    assert_eq!(
        node(&v, g("Chore"))["repeat"],
        "every 2 weeks, counted from completion"
    );
    assert!(node(&v, g("Foc2"))["repeat"].is_null());

    // Notes and unplanned.
    assert_eq!(node(&v, g("Noted"))["has_notes"], true);
    assert_eq!(node(&v, g("Noted"))["unplanned"], false);
    assert_eq!(node(&v, g("Bare"))["has_notes"], false);
    assert_eq!(node(&v, g("Bare"))["unplanned"], true);

    // Progress: counts children that are not dropped.
    assert_eq!(
        node(&v, g("Prog"))["progress"],
        json!({"done": 1, "total": 2})
    );
    assert!(node(&v, g("Bare"))["progress"].is_null());
    assert!(node(&v, g("Pri"))["progress"].is_null());

    // Timestamps are RFC 3339.
    assert!(
        node(&v, g("Bare"))["created_at"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn type_and_stage_are_in_node_and_tree() {
    let dir = temp_dir("type_stage");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    m.add(
        None,
        json!({"title": "Top", "kind": "area", "children": [
            {"title": "Typed"}, {"title": "Plain"}]}),
    )
    .await;
    let (top, typed, plain) = (
        id_of(&path, "Top"),
        id_of(&path, "Typed"),
        id_of(&path, "Plain"),
    );
    db(&path)
        .execute(
            "UPDATE nodes SET work_type = 'bug', stage = 'in_review' WHERE id = ?1",
            [typed],
        )
        .unwrap();

    let v = m.j(&format!("/api/nodes/{}", sid(typed))).await;
    assert_eq!(v["node"]["type"], "bug");
    assert_eq!(v["node"]["stage"], "in_review");
    let v = m.j(&format!("/api/nodes/{}", sid(plain))).await;
    assert!(v["node"]["type"].is_null());
    assert!(v["node"]["stage"].is_null());

    let v = m.j(&format!("/api/tree?root={top}&depth=1")).await;
    assert_eq!(node(&v, typed)["type"], "bug");
    assert_eq!(node(&v, typed)["stage"], "in_review");
    assert!(node(&v, plain)["type"].is_null());
    assert!(node(&v, plain)["stage"].is_null());

    server.shutdown().await;
}

#[tokio::test]
async fn node_cap_never_splits_a_group_of_children() {
    let dir = temp_dir("cap");
    let path = dir.join("spokes.db");
    let mut area_ids = Vec::new();
    seed(&path, |tx| {
        let mut pos = 0;
        for a in 0..3 {
            pos += 1;
            let area = ins(tx, 1, pos, "area", &format!("Area{a}"));
            area_ids.push(area);
            for t in 0..900 {
                let task = ins(tx, area, t, "task", &format!("T{a}-{t}"));
                ins(tx, task, 0, "task", &format!("T{a}-{t}-a"));
                ins(tx, task, 1, "task", &format!("T{a}-{t}-b"));
            }
        }
    });
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    let v = m.j("/api/tree").await;
    assert_eq!(v["truncated"], true);
    let n = v["nodes"].as_array().unwrap().len();
    assert!(n <= 2000, "{n} nodes");
    assert_eq!(node(&v, 1)["children_loaded"], true);
    assert_eq!(node(&v, 1)["child_count"], 3);
    check_whole_groups(&v, 1);
    // Breadth-first: both first areas fit, the third's children do not.
    assert_eq!(node(&v, area_ids[0])["children_loaded"], true);
    assert_eq!(node(&v, area_ids[1])["children_loaded"], true);
    assert_eq!(node(&v, area_ids[2])["children_loaded"], false);
    assert!(
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["children_loaded"] == false)
    );

    // gzip when asked; plain JSON otherwise.
    let r = m.get("/api/tree", &[("accept-encoding", "gzip")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers.get("content-encoding").unwrap(), "gzip");
    assert_eq!(&r.body[..2], &[0x1f, 0x8b]);
    let r = m.get("/api/tree", &[]).await;
    assert!(r.headers.get("content-encoding").is_none());
    assert_eq!(r.json()["truncated"], true);

    // The not-loaded node can be fetched as a root and then has all its children.
    let v = m.j(&format!("/api/tree?root={}", area_ids[2])).await;
    assert_eq!(node(&v, area_ids[2])["children_loaded"], true);
    let direct = v["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["parent"].as_i64() == Some(area_ids[2]))
        .count();
    assert_eq!(direct, 900);
    assert!(v["nodes"].as_array().unwrap().len() <= 2000);
    check_whole_groups(&v, area_ids[2]);

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);

    // A root with more direct children than fit: the first 1999, in order.
    let dir = temp_dir("cap-wide");
    let path = dir.join("spokes.db");
    let mut kids = Vec::new();
    seed(&path, |tx| {
        for i in 0..2500 {
            kids.push(ins(tx, 1, i, "task", &format!("W{i}")));
        }
    });
    let server = start(&path).await;
    let m = Mcp::new(server.addr);
    let v = m.j("/api/tree").await;
    assert_eq!(v["truncated"], true);
    assert_eq!(node(&v, 1)["children_loaded"], false);
    assert_eq!(node(&v, 1)["child_count"], 2500);
    let got = ids(&v);
    assert_eq!(got.len(), 2000);
    assert_eq!(got[0], 1);
    assert_eq!(&got[1..], &kids[..1999]);

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn node_detail_has_notes_path_and_recent_events() {
    let dir = temp_dir("detail");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    m.add(
        None,
        json!({"title": "Top", "kind": "area", "children": [
            {"title": "Mid", "children": [
                {"title": "WithNotes", "notes": "the body"},
                {"title": "NoNotes"}]}]}),
    )
    .await;
    let (top, mid, wn, nn) = (
        id_of(&path, "Top"),
        id_of(&path, "Mid"),
        id_of(&path, "WithNotes"),
        id_of(&path, "NoNotes"),
    );

    let v = m.j(&format!("/api/nodes/{}", sid(wn))).await;
    for k in [
        "rev",
        "version",
        "time_zone",
        "today",
        "path",
        "node",
        "events",
    ] {
        assert!(v.get(k).is_some(), "missing {k}");
    }
    assert_eq!(v["node"]["id"], wn);
    assert_eq!(v["node"]["notes"], "the body");
    assert_eq!(v["node"]["has_notes"], true);
    assert_eq!(v["node"]["parent"], mid);
    assert_eq!(
        v["path"],
        json!([
            {"id": 1, "title": "To do"},
            {"id": top, "title": "Top"},
            {"id": mid, "title": "Mid"}
        ])
    );

    let v = m.j(&format!("/api/nodes/{nn}")).await;
    assert!(v["node"]["notes"].is_null());
    assert_eq!(v["node"]["has_notes"], false);

    // More than ten writes: the last ten, newest first.
    for i in 0..12 {
        m.ok(
            "update",
            json!({"id": sid(nn), "title": "NoNotes", "notes_append": format!("n{i}"),
                   "agent": "ag1"}),
        )
        .await;
    }
    let v = m.j(&format!("/api/nodes/{nn}")).await;
    let ev = v["events"].as_array().unwrap();
    assert_eq!(ev.len(), 10);
    let eids: Vec<i64> = ev.iter().map(|e| e["id"].as_i64().unwrap()).collect();
    assert!(eids.windows(2).all(|w| w[0] > w[1]), "{eids:?}");
    for e in ev {
        for k in ["id", "at", "agent", "type", "detail"] {
            assert!(e.get(k).is_some(), "event lacks {k}");
        }
        assert!(e["at"].as_str().unwrap().ends_with('Z'));
        assert!(e["type"].is_string());
        assert_eq!(e["agent"], "ag1");
    }
    assert_eq!(
        v["node"]["notes"],
        (0..12)
            .map(|i| format!("n{i}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    );

    // Errors.
    let r = m.get("/api/nodes/SPOKE-99999", &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.json()["error"].is_string());
    let r = m.get("/api/nodes/abc", &[]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json()["error"].is_string());
    let r = m
        .get(&format!("/api/nodes/{nn}?include_closed=maybe"), &[])
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = m
        .get(&format!("/api/nodes/{nn}?include_closed=true"), &[])
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = m.get("/api/tree?root=SPOKE-99999", &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.json()["error"].is_string());

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn etag_and_not_modified() {
    let dir = temp_dir("etag");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    m.add(None, json!({"title": "Alpha", "kind": "area"})).await;
    let alpha = id_of(&path, "Alpha");

    let r = m.get("/api/tree", &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    let tag = r.etag();
    assert!(tag.starts_with("W/\""), "{tag}");
    assert_eq!(r.headers.get("cache-control").unwrap(), "no-cache");
    let rev = r.json()["rev"].as_i64().unwrap();

    // Matching tag: 304 with no body, in every form.
    for inm in [
        tag.clone(),
        tag.trim_start_matches("W/").to_string(),
        format!("\"other\", {tag}"),
        "*".to_string(),
    ] {
        let r = m.get("/api/tree", &[("if-none-match", &inm)]).await;
        assert_eq!(r.status, StatusCode::NOT_MODIFIED, "If-None-Match: {inm}");
        assert!(r.body.is_empty());
        assert_eq!(r.etag(), tag);
    }
    let r = m.get("/api/tree", &[("if-none-match", "\"nope\"")]).await;
    assert_eq!(r.status, StatusCode::OK);

    // A different query has a different tag, and the old tag does not match it.
    let other = m.get("/api/tree?depth=1", &[]).await;
    assert_ne!(other.etag(), tag);
    let r = m.get("/api/tree?depth=1", &[("if-none-match", &tag)]).await;
    assert_eq!(r.status, StatusCode::OK);
    let node_tag = m.get(&format!("/api/nodes/{alpha}"), &[]).await.etag();
    assert_ne!(node_tag, tag);

    // Any write changes the tag and the rev.
    m.add(None, json!({"title": "Beta"})).await;
    let r = m.get("/api/tree", &[("if-none-match", &tag)]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_ne!(r.etag(), tag);
    assert!(r.json()["rev"].as_i64().unwrap() > rev);
    let tag = r.etag();
    let r = m.get("/api/tree", &[("if-none-match", &tag)]).await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);

    // update and a write on a node change the node detail's tag too.
    let r = m.get(&format!("/api/nodes/{alpha}"), &[]).await;
    let node_tag = r.etag();
    m.ok("update", json!({"id": sid(alpha), "title": "Alpha2"}))
        .await;
    let r = m
        .get(
            &format!("/api/nodes/{alpha}"),
            &[("if-none-match", &node_tag)],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_ne!(r.etag(), node_tag);
    assert_eq!(r.json()["node"]["title"], "Alpha2");

    // claim is a write as well.
    m.add(None, json!({"title": "Gamma"})).await;
    let gamma = id_of(&path, "Gamma");
    let tag = m.get("/api/tree", &[]).await.etag();
    m.ok("claim", json!({"id": sid(gamma), "agent": "w1"}))
        .await;
    let r = m.get("/api/tree", &[("if-none-match", &tag)]).await;
    assert_eq!(r.status, StatusCode::OK);

    // A lease that runs out changes the tag with no write at all.
    let soon = ts_offset(2);
    db(&path)
        .execute(
            "UPDATE nodes SET claim_expires = ?1 WHERE id = ?2",
            rusqlite::params![soon, gamma],
        )
        .unwrap();
    let r = m.get("/api/tree", &[]).await;
    let tag = r.etag();
    assert_eq!(node(&r.json(), gamma)["stalled"], false);
    let r2 = m.get("/api/tree", &[("if-none-match", &tag)]).await;
    assert_eq!(r2.status, StatusCode::NOT_MODIFIED);
    tokio::time::sleep(Duration::from_millis(3200)).await;
    let r = m.get("/api/tree", &[("if-none-match", &tag)]).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "tag must change when the lease lapses"
    );
    assert_ne!(r.etag(), tag);
    assert_eq!(node(&r.json(), gamma)["stalled"], true);

    // Simulated lapse by moving the lease into the past.
    let tag = r.etag();
    db(&path)
        .execute(
            "UPDATE nodes SET claim_expires = '2020-01-01T00:00:00Z' WHERE id = ?1",
            [gamma],
        )
        .unwrap();
    let r = m.get("/api/tree", &[("if-none-match", &tag)]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(node(&r.json(), gamma)["stalled"], true);

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn origin_host_method_and_param_rules() {
    let dir = temp_dir("rules");

    // Default: no origins allowed, any host.
    let path = dir.join("one.db");
    let server = start(&path).await;
    let m = Mcp::new(server.addr);
    m.add(None, json!({"title": "Alpha"})).await;
    for p in ["/api/tree", "/api/nodes/1"] {
        let r = m.get(p, &[("origin", "http://view.lan:8080")]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{p}");
        assert!(r.json()["error"].is_string());
        let r = m.get(p, &[("origin", "null")]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{p}");
    }
    assert_eq!(m.get("/api/tree", &[]).await.status, StatusCode::OK);

    // 405 and unknown endpoints are JSON errors.
    let r = m.request("POST", "/api/tree", &[]).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(r.json()["error"].is_string());
    let r = m.request("DELETE", "/api/nodes/1", &[]).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(r.json()["error"].is_string());
    let r = m.get("/api/nope", &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.json()["error"].is_string());

    // Bad parameters.
    for q in [
        "depth=11",
        "depth=-1",
        "depth=x",
        "depth=",
        "include_closed=maybe",
        "foo=1",
        "depth=1&depth=2",
        "root=abc",
        "root=0",
        "root=1&root=1",
    ] {
        let r = m.get(&format!("/api/tree?{q}"), &[]).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "?{q}");
        assert!(r.json()["error"].is_string(), "?{q}");
    }
    for q in [
        "depth=10",
        "depth=0",
        "root=SPOKE-1",
        "root=1&depth=2&include_closed=1",
    ] {
        let r = m.get(&format!("/api/tree?{q}"), &[]).await;
        assert_eq!(r.status, StatusCode::OK, "?{q}");
    }
    server.shutdown().await;

    // An allowed origin passes; any other fails.
    let path = dir.join("two.db");
    let server = start_cfg(&path, None, vec!["http://view.lan:8080".into()]).await;
    let m = Mcp::new(server.addr);
    let r = m
        .get("/api/tree", &[("origin", "http://view.lan:8080")])
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = m
        .get("/api/tree", &[("origin", "http://evil.example")])
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.json()["error"].is_string());
    let r = m
        .get("/api/tree", &[("origin", "http://view.lan:9090")])
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    server.shutdown().await;

    // Allowed hosts.
    let path = dir.join("three.db");
    let server = start_cfg(&path, Some(vec!["spokes.lan".into()]), Vec::new()).await;
    let m = Mcp::new(server.addr);
    let host = format!("spokes.lan:{}", server.addr.port());
    let r = m.get("/api/tree", &[("host", &host)]).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = m.get("/api/tree", &[]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.json()["error"].is_string());
    let r = m.get("/api/tree", &[("host", "evil.example")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    server.shutdown().await;

    let _ = std::fs::remove_dir_all(dir);
}

fn dump(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| {
        Ok((0..n)
            .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
            .collect::<Vec<_>>()
            .join("|"))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

#[tokio::test]
async fn the_api_never_writes() {
    let dir = temp_dir("readonly");
    let path = dir.join("spokes.db");
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    m.add(
        None,
        json!({"title": "Area", "kind": "area", "children": [
            {"title": "Held", "notes": "n"}, {"title": "Lapsed"}, {"title": "Plain"}]}),
    )
    .await;
    let held = id_of(&path, "Held");
    let lapsed = id_of(&path, "Lapsed");
    m.ok("claim", json!({"id": sid(held), "agent": "w1"})).await;
    m.ok("claim", json!({"id": sid(lapsed), "agent": "w2"}))
        .await;
    db(&path)
        .execute(
            "UPDATE nodes SET claim_expires = '2020-01-01T00:00:00Z' WHERE id = ?1",
            [lapsed],
        )
        .unwrap();

    let conn = db(&path);
    let snapshot = |c: &rusqlite::Connection| {
        (
            dump(c, "SELECT * FROM nodes ORDER BY id"),
            dump(c, "SELECT * FROM events ORDER BY id"),
            dump(c, "SELECT * FROM notes ORDER BY node_id"),
        )
    };
    let before = snapshot(&conn);

    for p in [
        "/api/tree".to_string(),
        "/api/tree?depth=10&include_closed=true".to_string(),
        format!("/api/tree?root={held}"),
        format!("/api/nodes/{held}"),
        format!("/api/nodes/{lapsed}"),
        "/api/nodes/SPOKE-99999".to_string(),
        "/api/tree?bogus=1".to_string(),
    ] {
        let _ = m.get(&p, &[]).await;
        let _ = m.get(&p, &[("if-none-match", "*")]).await;
    }
    let v = m.j(&format!("/api/tree?root={held}")).await;
    assert_eq!(node(&v, held)["stalled"], false);
    assert_eq!(
        node(&m.j("/api/tree?depth=5").await, lapsed)["stalled"],
        true
    );

    assert_eq!(snapshot(&conn), before);

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn big_tree_is_fast() {
    let dir = temp_dir("perf");
    let path = dir.join("spokes.db");
    let mut project = 0;
    seed(&path, |tx| {
        for a in 0..10 {
            let area = ins(tx, 1, a, "area", &format!("Area{a}"));
            for p in 0..40 {
                let proj = ins(tx, area, p, "task", &format!("P{a}-{p}"));
                if a == 3 && p == 7 {
                    project = proj;
                }
                for t in 0..50 {
                    ins(tx, proj, t, "task", &format!("T{a}-{p}-{t}"));
                }
            }
        }
    });
    let server = start(&path).await;
    let m = Mcp::new(server.addr);

    let t = Instant::now();
    let r = m.get("/api/tree", &[]).await;
    let elapsed = t.elapsed();
    eprintln!("GET /api/tree on 20410 nodes: {elapsed:?}");
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["truncated"], true);
    assert!(v["nodes"].as_array().unwrap().len() <= 2000);
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    check_whole_groups(&v, 1);

    let t = Instant::now();
    let r = m
        .get(&format!("/api/tree?root={project}&depth=1"), &[])
        .await;
    let elapsed = t.elapsed();
    eprintln!("GET /api/tree?root=<project>&depth=1: {elapsed:?}");
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["nodes"].as_array().unwrap().len(), 51);
    assert_eq!(v["truncated"], false);
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}
