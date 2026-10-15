//! Prints what a REAPI server advertises (`GetCapabilities`): API versions,
//! digest functions, and the cache's features, chunked transfers included.
//!
//! `capabilities <grpc[s]://host:port> [--instance NAME]
//!   [--tls_client_certificate FILE --tls_client_key FILE] [--tls_certificate FILE]`

#![allow(clippy::disallowed_macros)] // It prints what it found.

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let endpoint = args.next().ok_or_else(|| anyhow::anyhow!("usage: capabilities <endpoint> [flags]"))?;
    let (mut instance, mut tls) = (String::new(), ninja_remote::Tls::default());
    while let Some(a) = args.next() {
        let mut value = || args.next().ok_or_else(|| anyhow::anyhow!("{a} needs a value"));
        match a.as_str() {
            "--instance" => instance = value()?,
            "--tls_client_certificate" => tls.client_cert = Some(std::fs::read(value()?)?),
            "--tls_client_key" => tls.client_key = Some(std::fs::read(value()?)?),
            "--tls_certificate" => tls.ca = Some(std::fs::read(value()?)?),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async {
        let mut client = ninja_remote::connect(&ninja_remote::ConnectOptions { endpoint, instance_name: instance, headers: Vec::new(), tls }).await?;
        let caps = client.capabilities().await?;
        let v = |v: Option<(i32, i32, i32)>| v.map_or("-".to_string(), |(a, b, c)| format!("{a}.{b}.{c}"));
        let low = caps.low_api_version.as_ref().map(|v| (v.major, v.minor, v.patch));
        let high = caps.high_api_version.as_ref().map(|v| (v.major, v.minor, v.patch));
        println!("api versions: {} to {}", v(low), v(high));
        if let Some(c) = &caps.cache_capabilities {
            println!("digest functions: {:?}", c.digest_functions);
            println!("action cache updates: {:?}", c.action_cache_update_capabilities.as_ref().map(|u| u.update_enabled));
            println!("max batch total size: {} bytes", c.max_batch_total_size_bytes);
            println!("compressors: {:?}", c.supported_compressors);
            println!("split blobs: {}, splice blobs: {}", c.split_blob_support, c.splice_blob_support);
            println!("chunking: {c:#?}");
        } else {
            println!("no cache capabilities");
        }
        println!("execution: {:?}", caps.execution_capabilities.as_ref().map(|e| e.exec_enabled));
        anyhow::Ok(())
    })
}
