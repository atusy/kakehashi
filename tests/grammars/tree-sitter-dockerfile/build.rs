fn main() {
    let src_dir = std::path::Path::new("src");
    let mut c_config = cc::Build::new();
    c_config.std("c11").include(src_dir);
    c_config.file(src_dir.join("parser.c"));
    c_config.file(src_dir.join("scanner.c"));
    c_config.compile("tree-sitter-dockerfile");
}
