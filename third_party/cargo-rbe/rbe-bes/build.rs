//! Generates the BES service (googleapis `google.devtools.build.v1`) and
//! Bazel's Build Event Protocol (`build_event_stream`) from the vendored
//! protos in `proto/`. protox compiles them, so no system protoc is needed.

#![allow(clippy::disallowed_macros)] // Cargo reads its directives on stdout.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = [
        "google/devtools/build/v1/publish_build_event.proto",
        "src/main/java/com/google/devtools/build/lib/buildeventstream/proto/build_event_stream.proto",
    ];
    let fds = protox::compile(files, ["proto"])?;
    tonic_prost_build::configure()
        .build_client(true)
        // The server side is only used by tests (a fake BES backend).
        .build_server(true)
        .include_file("protos.rs")
        .compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
