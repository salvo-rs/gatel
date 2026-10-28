//! File-only server TLS edits travel through the ordinary conditional reload.
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::*;

struct Fixture {
    app: Arc<App>,
    directory: PathBuf,
    address: SocketAddr,
}

impl Fixture {
    fn new(client_auth: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "gatel-server-refresh-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let app = Arc::new(App::new(directory.join("gatel.kdl")).unwrap());
        let address = app.binds.adopt(listener).unwrap();
        let auth = if client_auth {
            "{ client-auth \"required\" ca=\"clients.pem\"; }"
        } else {
            ""
        };
        std::fs::write(&app.root, format!(
            "global {{ https \"{address}\"; }}\nsite \"server.internal\" {{ tls cert=\"server.pem\" key=\"server.key\" {auth}\nroute \"/\" {{ respond \"ok\"; }} }}"
        )).unwrap();
        Self {
            app,
            directory,
            address,
        }
    }

    fn client(&self, identity: Option<&str>) -> gatel_tls::ClientTls {
        gatel_tls::ClientTls::new(
            &gatel_plan::UpstreamTls {
                server_name: "server.internal".into(),
                ca: Some(gatel_model::SecretRef::parse("trust.pem").unwrap()),
                client_cert: identity
                    .map(|name| gatel_model::SecretRef::parse(&format!("{name}.pem")).unwrap()),
                client_key: identity
                    .map(|name| gatel_model::SecretRef::parse(&format!("{name}.key")).unwrap()),
            },
            gatel_plan::UpstreamProtocol::Http1,
            &self.directory,
        )
        .unwrap()
    }

    async fn served(&self, client: &gatel_tls::ClientTls) -> Result<Vec<u8>, String> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let stream = tokio::net::TcpStream::connect(self.address)
                .await
                .map_err(|e| e.to_string())?;
            let mut stream = client.connect(stream).await?;
            let peer = stream
                .get_ref()
                .1
                .peer_certificates()
                .ok_or("no server certificate")?[0]
                .to_vec();
            exchange(&mut stream, true).await?;
            Ok(peer)
        })
        .await
        .map_err(|_| "TLS exchange timed out".to_string())?
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn pair(directory: &Path, name: &str, hostname: &str) -> (String, Vec<u8>) {
    let pair = rcgen::generate_simple_self_signed(vec![hostname.to_string()]).unwrap();
    let pem = pair.cert.pem();
    std::fs::write(directory.join(format!("{name}.pem")), &pem).unwrap();
    write_key(
        &directory.join(format!("{name}.key")),
        &pair.signing_key.serialize_pem(),
    );
    (pem, pair.cert.der().to_vec())
}

fn write_key(path: &Path, key: &str) {
    std::fs::write(path, key).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

async fn exchange(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), close: bool) -> Result<(), String> {
    let close = if close { "close" } else { "keep-alive" };
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: server.internal\r\nConnection: {close}\r\n\r\n").as_bytes(),
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    let mut byte = [0];
    while !response.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.map_err(|e| e.to_string())?;
        response.push(byte[0]);
        if response.len() > 8192 {
            return Err("oversized response headers".into());
        }
    }
    if !response.starts_with(b"HTTP/1.1 200") {
        return Err("request was refused".into());
    }
    let mut body = [0; 2];
    stream.read_exact(&mut body).await.map_err(|e| e.to_string())?;
    if body != *b"ok" {
        return Err("unexpected response body".into());
    }
    Ok(())
}

#[tokio::test]
async fn unchanged_config_refreshes_manual_pair_and_keeps_serving_after_bad_edits() {
    let fixture = Fixture::new(false);
    let (first_pem, first_der) = pair(&fixture.directory, "server", "server.internal");
    let (second_pem, second_der) = pair(&fixture.directory, "next", "server.internal");
    let second_key = std::fs::read_to_string(fixture.directory.join("next.key")).unwrap();
    std::fs::write(
        fixture.directory.join("trust.pem"),
        format!("{first_pem}{second_pem}"),
    )
    .unwrap();
    assert!(matches!(
        crate::watch::reload_once(&fixture.app).await,
        Outcome::Started { .. }
    ));
    let client = fixture.client(None);
    assert_eq!(fixture.served(&client).await.unwrap(), first_der);
    let initial = fixture.app.store.current().unwrap();
    let revs = fixture.app.store.status().revs;
    assert!(matches!(
        crate::watch::reload_once(&fixture.app).await,
        Outcome::NoOp { .. }
    ));

    std::fs::write(fixture.directory.join("server.pem"), second_pem).unwrap();
    assert!(matches!(
        crate::watch::reload_once(&fixture.app).await,
        Outcome::PrepareFailed(_)
    ));
    assert!(Arc::ptr_eq(&initial, &fixture.app.store.current().unwrap()));
    assert_eq!(fixture.served(&client).await.unwrap(), first_der);
    write_key(&fixture.directory.join("server.key"), &second_key);
    assert!(matches!(
        crate::watch::reload_once(&fixture.app).await,
        Outcome::Reloaded { .. }
    ));
    let current = fixture.app.store.current().unwrap();
    assert_eq!(current.plan_hash(), initial.plan_hash());
    assert_eq!(fixture.app.store.status().revs, revs);
    assert_eq!(fixture.served(&client).await.unwrap(), second_der);

    for name in ["server.pem", "server.key"] {
        let path = fixture.directory.join(name);
        let original = std::fs::read(&path).unwrap();
        std::fs::write(&path, "invalid material").unwrap();
        assert!(matches!(
            crate::watch::reload_once(&fixture.app).await,
            Outcome::PrepareFailed(_)
        ));
        assert!(Arc::ptr_eq(&current, &fixture.app.store.current().unwrap()));
        assert_eq!(fixture.served(&client).await.unwrap(), second_der);
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            crate::watch::reload_once(&fixture.app).await,
            Outcome::PrepareFailed(_)
        ));
        std::fs::write(&path, original).unwrap();
        #[cfg(unix)]
        if name.ends_with(".key") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(matches!(
            crate::watch::reload_once(&fixture.app).await,
            Outcome::NoOp { .. }
        ));
    }
    fixture.app.shutdown().await;
}

#[tokio::test]
async fn unchanged_config_refreshes_inbound_client_ca_without_revoking_old_connections() {
    let fixture = Fixture::new(true);
    let (server_pem, _) = pair(&fixture.directory, "server", "server.internal");
    let (first_pem, _) = pair(&fixture.directory, "first-client", "first.internal");
    let (second_pem, _) = pair(&fixture.directory, "second-client", "second.internal");
    std::fs::write(fixture.directory.join("trust.pem"), server_pem).unwrap();
    let ca = fixture.directory.join("clients.pem");
    std::fs::write(&ca, &first_pem).unwrap();
    assert!(matches!(
        crate::watch::reload_once(&fixture.app).await,
        Outcome::Started { .. }
    ));
    let first = fixture.client(Some("first-client"));
    let second = fixture.client(Some("second-client"));
    assert!(fixture.served(&first).await.is_ok());
    assert!(fixture.served(&second).await.is_err());
    let initial = fixture.app.store.current().unwrap();
    let revs = fixture.app.store.status().revs;
    let stream = tokio::net::TcpStream::connect(fixture.address).await.unwrap();
    let mut established = first.connect(stream).await.unwrap();
    exchange(&mut established, false).await.unwrap();
    std::fs::write(&ca, "invalid CA").unwrap();
    assert!(matches!(
        crate::watch::reload_once(&fixture.app).await,
        Outcome::PrepareFailed(_)
    ));
    assert!(Arc::ptr_eq(&initial, &fixture.app.store.current().unwrap()));
    assert!(fixture.served(&first).await.is_ok());
    std::fs::remove_file(&ca).unwrap();
    assert!(matches!(
        crate::watch::reload_once(&fixture.app).await,
        Outcome::PrepareFailed(_)
    ));
    std::fs::write(&ca, &second_pem).unwrap();
    assert!(matches!(
        crate::watch::reload_once(&fixture.app).await,
        Outcome::Reloaded { .. }
    ));
    assert_eq!(
        fixture.app.store.current().unwrap().plan_hash(),
        initial.plan_hash()
    );
    assert_eq!(fixture.app.store.status().revs, revs);
    assert!(fixture.served(&first).await.is_err());
    assert!(fixture.served(&second).await.is_ok());
    tokio::time::timeout(Duration::from_secs(2), exchange(&mut established, true))
        .await
        .unwrap()
        .unwrap();
    drop(established);
    assert!(matches!(
        crate::watch::reload_once(&fixture.app).await,
        Outcome::NoOp { .. }
    ));
    fixture.app.shutdown().await;
}
