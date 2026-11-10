//! Bolt endpoint over a real TCP socket and a real store (no driver needed).
//! The official Neo4j drivers are exercised manually; see docs/wiki/Cypher.md.

use ehgraph::bolt::{decode, encode, serve, BoltOptions, Pack};
use ehgraph_server::{boot, Config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn start() -> (std::net::SocketAddr, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ehgraph.toml");
    std::fs::write(
        &path,
        format!(
            r#"
instance_id = "ehgraph-bolt-test"
domain = "bolt-test"
data_dir = "{data}"
backup_prefix = "{backup}"
listen = "127.0.0.1:9"
role = "primary"
build = "test"
backup_interval_s = 0

[auth]
mode = "bearer"

[[auth.grants]]
token = "lab-writer"
principal = "fixture"
namespace = "lab"
read = true
write = true
"#,
            data = dir.path().join("data").display(),
            backup = dir.path().join("backup").display(),
        ),
    )
    .unwrap();
    let running = boot(Config::load(&path).unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        running.graph.clone(),
        listener,
        BoltOptions {
            default_namespace: Some("lab".into()),
        },
    ));
    (addr, dir)
}

struct Client(TcpStream);

impl Client {
    async fn connect(addr: std::net::SocketAddr, proposals: [u8; 16]) -> (Self, [u8; 4]) {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(&[0x60, 0x60, 0xB0, 0x17]).await.unwrap();
        s.write_all(&proposals).await.unwrap();
        let mut agreed = [0u8; 4];
        s.read_exact(&mut agreed).await.unwrap();
        (Client(s), agreed)
    }

    async fn send(&mut self, sig: u8, fields: Vec<Pack>) {
        let mut body = Vec::new();
        encode(&Pack::Struct(sig, fields), &mut body);
        let mut framed = (body.len() as u16).to_be_bytes().to_vec();
        framed.extend(body);
        framed.extend([0, 0]);
        self.0.write_all(&framed).await.unwrap();
    }

    async fn recv(&mut self) -> (u8, Vec<Pack>) {
        let mut msg = Vec::new();
        loop {
            let mut h = [0u8; 2];
            self.0.read_exact(&mut h).await.unwrap();
            let n = u16::from_be_bytes(h) as usize;
            if n == 0 {
                break;
            }
            let start = msg.len();
            msg.resize(start + n, 0);
            self.0.read_exact(&mut msg[start..]).await.unwrap();
        }
        let mut pos = 0;
        match decode(&msg, &mut pos).unwrap() {
            Pack::Struct(sig, fields) => (sig, fields),
            other => panic!("not a message: {other:?}"),
        }
    }

    /// RUN + PULL, returning (records, final summary).
    async fn query(&mut self, q: &str, params: Pack) -> (Vec<Vec<Pack>>, Pack) {
        self.send(0x10, vec![Pack::Str(q.into()), params, Pack::Map(vec![])])
            .await;
        let (sig, f) = self.recv().await;
        assert_eq!(sig, 0x70, "RUN failed: {f:?}");
        self.send(0x3F, vec![Pack::Map(vec![("n".into(), Pack::Int(-1))])])
            .await;
        let mut records = Vec::new();
        loop {
            let (sig, f) = self.recv().await;
            match sig {
                0x71 => match &f[0] {
                    Pack::List(values) => records.push(values.clone()),
                    other => panic!("{other:?}"),
                },
                0x70 => return (records, f[0].clone()),
                _ => panic!("PULL failed: {f:?}"),
            }
        }
    }
}

fn hello(token: &str) -> Vec<Pack> {
    vec![Pack::Map(vec![
        ("user_agent".into(), Pack::Str("test/1".into())),
        ("scheme".into(), Pack::Str("basic".into())),
        ("principal".into(), Pack::Str("anyone".into())),
        ("credentials".into(), Pack::Str(token.into())),
    ])]
}

fn field<'a>(map: &'a Pack, key: &str) -> &'a Pack {
    match map {
        Pack::Map(entries) => &entries.iter().find(|(k, _)| k == key).unwrap().1,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn bolt_five_runs_cypher_with_element_ids() {
    let (addr, _dir) = start().await;
    // Proposals as a 5.x driver sends them: 5.4 (range 4), 4.4 (range 2).
    let (mut c, agreed) =
        Client::connect(addr, [0, 4, 4, 5, 0, 2, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0]).await;
    assert_eq!(agreed, [0, 0, 0, 5]);
    c.send(0x01, hello("lab-writer")).await;
    let (sig, f) = c.recv().await;
    assert_eq!(sig, 0x70);
    assert!(
        matches!(field(&f[0], "server"), Pack::Str(s) if s.starts_with("Neo4j/") && s.contains("ehgraph"))
    );

    let (_, summary) = c
        .query(
            "MERGE (g:Gateway {id: $gw}) MERGE (a:AccessPoint {id: 'ap:1', name: 'U7'}) MERGE (a)-[:UPLINK]->(g)",
            Pack::Map(vec![("gw".into(), Pack::Str("gw:1".into()))]),
        )
        .await;
    assert_eq!(
        field(field(&summary, "stats"), "nodes-created"),
        &Pack::Int(2)
    );

    let (records, summary) = c
        .query(
            "MATCH (a:AccessPoint)-[r]->(g) RETURN a, r, a.name",
            Pack::Null,
        )
        .await;
    assert_eq!(field(&summary, "type"), &Pack::Str("r".into()));
    assert_eq!(records.len(), 1);
    let Pack::Struct(0x4E, node) = &records[0][0] else {
        panic!("{:?}", records[0][0])
    };
    assert_eq!(node[1], Pack::List(vec![Pack::Str("AccessPoint".into())]));
    assert_eq!(
        node[3],
        Pack::Str("ap:1".into()),
        "Bolt 5 element_id is the ehgraph id"
    );
    let Pack::Struct(0x52, rel) = &records[0][1] else {
        panic!()
    };
    assert_eq!(rel[3], Pack::Str("UPLINK".into()));
    assert_eq!(rel[6], Pack::Str("ap:1".into()));
    assert_eq!(rel[7], Pack::Str("gw:1".into()));
    assert_eq!(records[0][2], Pack::Str("U7".into()));

    // Errors put the connection in FAILED until RESET; later messages are IGNORED.
    c.send(
        0x10,
        vec![
            Pack::Str("MATC (n) RETURN n".into()),
            Pack::Map(vec![]),
            Pack::Map(vec![]),
        ],
    )
    .await;
    let (sig, f) = c.recv().await;
    assert_eq!(sig, 0x7F);
    assert_eq!(
        field(&f[0], "code"),
        &Pack::Str("Neo.ClientError.Statement.SyntaxError".into())
    );
    c.send(0x3F, vec![Pack::Map(vec![])]).await;
    assert_eq!(c.recv().await.0, 0x7E);
    c.send(0x0F, vec![]).await;
    assert_eq!(c.recv().await.0, 0x70);

    // Explicit transaction: a read-only one rolls back cleanly.
    c.send(0x11, vec![Pack::Map(vec![])]).await;
    assert_eq!(c.recv().await.0, 0x70);
    let (records, _) = c.query("MATCH (n) RETURN count(n)", Pack::Null).await;
    assert_eq!(records, vec![vec![Pack::Int(2)]]);
    c.send(0x13, vec![]).await;
    assert_eq!(c.recv().await.0, 0x70);
    // One that wrote rolls back too: the write never reaches the store.
    c.send(0x11, vec![Pack::Map(vec![])]).await;
    assert_eq!(c.recv().await.0, 0x70);
    c.query("CREATE (:Probe {id: 'p'})", Pack::Null).await;
    let (records, _) = c.query("MATCH (p:Probe) RETURN count(p)", Pack::Null).await;
    assert_eq!(
        records,
        vec![vec![Pack::Int(1)]],
        "read-your-writes in the tx"
    );
    c.send(0x13, vec![]).await;
    let (sig, f) = c.recv().await;
    assert_eq!(sig, 0x70, "{f:?}");
    let (records, _) = c.query("MATCH (p:Probe) RETURN count(p)", Pack::Null).await;
    assert_eq!(records, vec![vec![Pack::Int(0)]]);
}

/// The bookmark's graph version.
fn bookmark_version(summary: &Pack) -> u64 {
    match field(summary, "bookmark") {
        Pack::Str(b) => b.rsplit(':').next().unwrap().parse().unwrap(),
        other => panic!("{other:?}"),
    }
}

async fn begin(c: &mut Client) {
    c.send(0x11, vec![Pack::Map(vec![])]).await;
    let (sig, f) = c.recv().await;
    assert_eq!(sig, 0x70, "BEGIN: {f:?}");
}

async fn commit(c: &mut Client) -> (u8, Vec<Pack>) {
    c.send(0x12, vec![]).await;
    c.recv().await
}

#[tokio::test]
async fn bolt_multi_statement_transactions_are_atomic() {
    let (addr, _dir) = start().await;
    let five = [0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let (mut c, _) = Client::connect(addr, five).await;
    c.send(0x01, hello("lab-writer")).await;
    assert_eq!(c.recv().await.0, 0x70);
    let (_, summary) = c
        .query(
            "CREATE (:Account {id: 'a', balance: 100}), (:Account {id: 'b', balance: 0})",
            Pack::Null,
        )
        .await;
    let v0 = bookmark_version(&summary);

    // Two statements, one COMMIT, one new version.
    begin(&mut c).await;
    c.query(
        "MATCH (a:Account {id: 'a'}) SET a.balance = a.balance - 40",
        Pack::Null,
    )
    .await;
    c.query(
        "MATCH (b:Account {id: 'b'}) SET b.balance = b.balance + 40",
        Pack::Null,
    )
    .await;
    // Another connection does not see uncommitted writes.
    let (mut other, _) = Client::connect(addr, five).await;
    other.send(0x01, hello("lab-writer")).await;
    assert_eq!(other.recv().await.0, 0x70);
    let (records, _) = other
        .query(
            "MATCH (a:Account) RETURN a.balance ORDER BY a.id",
            Pack::Null,
        )
        .await;
    assert_eq!(records, vec![vec![Pack::Int(100)], vec![Pack::Int(0)]]);
    let (sig, f) = commit(&mut c).await;
    assert_eq!(sig, 0x70, "{f:?}");
    assert_eq!(bookmark_version(&f[0]), v0 + 1);
    let (records, _) = other
        .query(
            "MATCH (a:Account) RETURN a.balance ORDER BY a.id",
            Pack::Null,
        )
        .await;
    assert_eq!(records, vec![vec![Pack::Int(60)], vec![Pack::Int(40)]]);

    // A dropped connection rolls its open transaction back.
    {
        let (mut d, _) = Client::connect(addr, five).await;
        d.send(0x01, hello("lab-writer")).await;
        assert_eq!(d.recv().await.0, 0x70);
        begin(&mut d).await;
        d.query("CREATE (:Probe {id: 'dropped'})", Pack::Null).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (records, _) = other
        .query("MATCH (p:Probe) RETURN count(p)", Pack::Null)
        .await;
    assert_eq!(records, vec![vec![Pack::Int(0)]]);

    // A lost race fails COMMIT with a transient code that drivers retry.
    begin(&mut c).await;
    c.query(
        "MATCH (a:Account {id: 'a'}) SET a.balance = a.balance + 1",
        Pack::Null,
    )
    .await;
    other
        .query("MATCH (a:Account {id: 'a'}) SET a.balance = 0", Pack::Null)
        .await;
    let (sig, f) = commit(&mut c).await;
    assert_eq!(sig, 0x7F, "{f:?}");
    assert_eq!(
        field(&f[0], "code"),
        &Pack::Str("Neo.TransientError.Transaction.Outdated".into())
    );
    c.send(0x0F, vec![]).await;
    assert_eq!(c.recv().await.0, 0x70);
    let (records, _) = c
        .query("MATCH (a:Account {id: 'a'}) RETURN a.balance", Pack::Null)
        .await;
    assert_eq!(records, vec![vec![Pack::Int(0)]]);

    // A failed statement fails the transaction; RESET discards it.
    begin(&mut c).await;
    c.query("CREATE (:Probe {id: 'p1'})", Pack::Null).await;
    c.send(
        0x10,
        vec![
            Pack::Str("MATCH (n RETURN n".into()),
            Pack::Null,
            Pack::Map(vec![]),
        ],
    )
    .await;
    assert_eq!(c.recv().await.0, 0x7F);
    c.send(0x0F, vec![]).await;
    assert_eq!(c.recv().await.0, 0x70);
    let (records, _) = c.query("MATCH (p:Probe) RETURN count(p)", Pack::Null).await;
    assert_eq!(records, vec![vec![Pack::Int(0)]]);

    // tx_metadata.expect: commit only if the graph is still at the expected version.
    let (_, summary) = c.query("RETURN 1", Pack::Null).await;
    let now = bookmark_version(&summary);
    let expect = |v: i64| {
        Pack::Map(vec![(
            "tx_metadata".into(),
            Pack::Map(vec![(
                "expect".into(),
                Pack::Map(vec![("graph_version".into(), Pack::Int(v))]),
            )]),
        )])
    };
    c.send(0x11, vec![expect(now as i64 - 1)]).await;
    assert_eq!(c.recv().await.0, 0x70);
    c.query("CREATE (:Probe {id: 'stale'})", Pack::Null).await;
    let (sig, f) = commit(&mut c).await;
    assert_eq!(sig, 0x7F, "{f:?}");
    c.send(0x0F, vec![]).await;
    assert_eq!(c.recv().await.0, 0x70);
    c.send(0x11, vec![expect(now as i64)]).await;
    assert_eq!(c.recv().await.0, 0x70);
    c.query("CREATE (:Probe {id: 'fresh'})", Pack::Null).await;
    let (sig, f) = commit(&mut c).await;
    assert_eq!(sig, 0x70, "{f:?}");
    let (records, _) = c.query("MATCH (p:Probe) RETURN p.id", Pack::Null).await;
    assert_eq!(records, vec![vec![Pack::Str("fresh".into())]]);
}

#[tokio::test]
async fn bolt_four_four_and_auth() {
    let (addr, _dir) = start().await;
    let (mut c, agreed) =
        Client::connect(addr, [0, 2, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).await;
    assert_eq!(agreed, [0, 0, 4, 4]);
    c.send(0x01, hello("lab-writer")).await;
    assert_eq!(c.recv().await.0, 0x70);
    c.query("CREATE (:N {id: 'n1'})", Pack::Null).await;
    let (records, _) = c.query("MATCH (n:N) RETURN n", Pack::Null).await;
    let Pack::Struct(0x4E, node) = &records[0][0] else {
        panic!()
    };
    assert_eq!(node.len(), 3, "Bolt 4.4 nodes have no element_id");

    // Unsupported versions get 0; a bad token is refused at HELLO.
    let (_, agreed) = Client::connect(addr, [0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).await;
    assert_eq!(agreed, [0, 0, 0, 0]);
    let (mut c, _) = Client::connect(addr, [0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).await;
    c.send(0x01, hello("wrong")).await;
    let (sig, f) = c.recv().await;
    assert_eq!(sig, 0x7F);
    assert_eq!(
        field(&f[0], "code"),
        &Pack::Str("Neo.ClientError.Security.Unauthorized".into())
    );
}
