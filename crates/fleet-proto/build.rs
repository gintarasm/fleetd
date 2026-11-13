fn main() {
    let fds = protox::compile(["proto/fleet.proto"], ["proto"]).expect("compile proto");
    prost_build::Config::new()
        // `Bytes` lines: the node hands out slices of one buffer, no copies.
        .bytes([".fleet.v1.AttachOk.history_tail"])
        .compile_fds(fds)
        .expect("generate prost types");
    println!("cargo:rerun-if-changed=proto/fleet.proto");
}
