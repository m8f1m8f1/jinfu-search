fn main() {
    let _ = embed_resource::compile("assets/icon.rc", embed_resource::NONE);
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
