//! 稳定的非加密内容指纹；安全认证与文件身份校验使用各自的 SHA-256 边界。

/// 以 FNV-1a 生成确定性十六进制指纹，供运行态与资源内容比较。
pub(crate) fn deterministic_hash(input: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3_u64);
    }
    format!("{hash:016x}")
}
