use std::env;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use walkdir::WalkDir;

fn main() {
    println!("cargo:rerun-if-changed=../../testcases");

    let out_dir = env::var("OUT_DIR").unwrap();
    let out_test = Path::new(&out_dir).join("test.rs");
    let mut out_test = File::create(&out_test).unwrap();

    for entry in WalkDir::new("../../testcases") {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            let file = entry.path().file_stem().unwrap().to_string_lossy();
            let _ = write!(out_test, "#[test]\n");
            let _ = write!(out_test, "fn test_{}() {{\n", file);
            let _ = write!(out_test, "    test(\"{}\");\n", file);
            let _ = write!(out_test, "}}\n");
        }
    }
}
