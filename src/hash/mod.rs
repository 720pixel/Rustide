pub mod direct;
pub mod reader;
pub mod sha1;

pub fn backend() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512bw") {
            return "avx512";
        }
        if std::is_x86_feature_detected!("avx2") {
            return "avx2";
        }
        if std::is_x86_feature_detected!("sha") {
            return "sha";
        }
    }
    "portable"
}
