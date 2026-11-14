fn main() {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let locales_dir = std::path::Path::new("locales");
    rune_i18n::codegen::generate(locales_dir, &out_dir).expect("generate rune-i18n catalogs");
}
