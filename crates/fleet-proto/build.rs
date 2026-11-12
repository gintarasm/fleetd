fn main() {
    let fds = protox::compile(["proto/fleet.proto"], ["proto"]).expect("compile proto");
    prost_build::Config::new()
        .compile_fds(fds)
        .expect("generate prost types");
    println!("cargo:rerun-if-changed=proto/fleet.proto");
}
