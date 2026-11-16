fn main() {
    let fds = protox::compile(["proto/fleet.proto", "proto/handoff.proto"], ["proto"])
        .expect("compile proto");
    prost_build::Config::new()
        // `Bytes` lines: the node hands out slices of one buffer, no copies.
        .bytes([
            ".fleet.v1.AttachOk.history_tail",
            ".fleet.v1.HistoryPage.lines",
        ])
        // Large oneof variants are boxed so `Frame` stays small: it is moved and
        // initialised on every decode (see the `frame_decode` benchmark).
        .boxed(".fleet.v1.Request.kind.spawn")
        .boxed(".fleet.v1.Request.kind.forget")
        .boxed(".fleet.v1.Request.kind.acquire_lease")
        .boxed(".fleet.v1.Response.kind.lease_granted")
        .boxed(".fleet.v1.Response.kind.spawned")
        .boxed(".fleet.v1.Response.kind.attached")
        .boxed(".fleet.v1.Response.kind.spawned_attached")
        .boxed(".fleet.v1.Response.kind.restarted")
        .boxed(".fleet.v1.HelloOk.binary")
        .boxed(".fleet.v1.HelloOk.restore")
        .compile_fds(fds)
        .expect("generate prost types");
    println!("cargo:rerun-if-changed=proto/fleet.proto");
    println!("cargo:rerun-if-changed=proto/handoff.proto");
}
