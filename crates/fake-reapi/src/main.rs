//! Runs the fake REAPI server standalone (manual testing):
//! `fake-reapi-server [--port N] [--work DIR] [--tls-cert PEM --tls-key PEM
//! [--tls-client-ca PEM]]` (with a client CA, clients need a certificate
//! it signed).

use std::path::PathBuf;

fn main() {
    let mut port = 0u16;
    let mut work = std::env::temp_dir().join(format!("fake-reapi-{}", std::process::id()));
    let (mut cert, mut key, mut client_ca) = (None, None, None);
    let mut args = std::env::args().skip(1);
    let read = |p: Option<String>, what: &str| std::fs::read(p.unwrap_or_else(|| panic!("{what} PEM"))).unwrap_or_else(|e| panic!("{what}: {e}"));
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).expect("--port N"),
            "--work" => work = PathBuf::from(args.next().expect("--work DIR")),
            "--tls-cert" => cert = Some(read(args.next(), "--tls-cert")),
            "--tls-key" => key = Some(read(args.next(), "--tls-key")),
            "--tls-client-ca" => client_ca = Some(read(args.next(), "--tls-client-ca")),
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    rt.block_on(async move {
        let tls = match (cert, key) {
            (Some(cert_pem), Some(key_pem)) => Some(fake_reapi::ServerTls { cert_pem, key_pem, client_ca_pem: client_ca }),
            (None, None) => None,
            _ => panic!("--tls-cert and --tls-key go together"),
        };
        let server = fake_reapi::FakeServer::start_with(&work, port, tls).await.expect("starting server");
        println!("{}", server.endpoint);
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("{:?}", server.stats());
        // Which invocations the executions were for.
        let mut invocations: std::collections::BTreeMap<String, usize> = Default::default();
        for m in server.state.request_metadata.lock().unwrap().iter() {
            *invocations.entry(m.tool_invocation_id.clone()).or_default() += 1;
        }
        eprintln!("executions per invocation: {invocations:?}");
    });
}
