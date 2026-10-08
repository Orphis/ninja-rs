//! Runs the fake REAPI server standalone (manual testing):
//! `fake-reapi-server [--port N] [--work DIR]`.

use std::path::PathBuf;

fn main() {
    let mut port = 0u16;
    let mut work = std::env::temp_dir().join(format!("fake-reapi-{}", std::process::id()));
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().and_then(|v| v.parse().ok()).expect("--port N"),
            "--work" => work = PathBuf::from(args.next().expect("--work DIR")),
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    rt.block_on(async move {
        let server = fake_reapi::FakeServer::start_on(&work, port).await.expect("starting server");
        println!("{}", server.endpoint);
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("{:?}", server.stats());
    });
}
