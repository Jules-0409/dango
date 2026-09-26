/// 校验 GCM 认证 tag，然后用 AES-256-CTR 解密 Droid CLI 写出的登录态（AAD 为空）。
///
/// 手搓 GCM 是有正当理由的：Droid CLI 是 Node.js 写的，IV 是 16 字节，
/// 而 `aes-gcm` crate 的 `Aes256Gcm` 把 nonce 类型固定成 `Nonce<U12>`（12 字节），处理不了。
/// 这里按 NIST SP 800-38D 从 16 字节 IV 推导 J0。
pub(super) fn decrypt_factory_payload(
    key_bytes: &[u8],
    iv: &[u8],
    tag: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, String> {
    use aes::cipher::{BlockEncrypt, KeyInit};
    use aes::Aes256;
    use ctr::cipher::{KeyIvInit, StreamCipher};
    use ghash::universal_hash::generic_array::GenericArray;
    use ghash::universal_hash::UniversalHash;
    use ghash::GHash;

    type Aes256Ctr32BE = ctr::Ctr32BE<Aes256>;

    // 兼容 NIST SP 800-38D / Node.js OpenSSL:
    // 当 IV 为 16 字节时，通过 GHASH 计算初始计数器 J0，再用 CTR 解密密文

    // 0. H = AES_K(0^128)：GCM 的哈希子密钥，J0 推导和认证 tag 都要用
    let cipher_block =
        Aes256::new_from_slice(key_bytes).map_err(|e| format!("Invalid AES key: {}", e))?;
    let mut h = [0u8; 16];
    cipher_block.encrypt_block((&mut h).into());

    if iv.len() != 12 && iv.len() < 16 {
        return Err(format!(
            "Invalid Factory credential format; unsupported iv length {} bytes (expected 12 or >= 16)",
            iv.len()
        ));
    }

    let j0 = if iv.len() == 12 {
        let mut j = [0u8; 16];
        j[0..12].copy_from_slice(iv);
        j[15] = 1;
        j
    } else {
        // 1. 计算 J0 = GHASH_H(IV || 0^s || [len(IV)]_64)，s 把 IV 补零到 128 位边界
        //    （NIST SP 800-38D 5.2.1.2）。原来的写法有两个 bug：
        //      - 只喂了 IV 的前 16 字节，超过 16 字节的部分被丢掉；
        //      - 长度块写成 `b2[15] = (len*8) as u8`，IV ≥ 32 字节时 256 被截成 0。
        //    16 字节 IV 恰好两处都不受影响（128 落在最低字节、IV 也只有一块），
        //    所以实测一直是对的；但 32 字节的 IV 会解不开。
        let mut ghash = GHash::new(GenericArray::from_slice(&h));
        ghash.update_padded(iv); // IV 的全部字节，末尾按需补零到块边界

        let mut b2_bytes = [0u8; 16];
        // 前 8 字节是 AAD 长度（此处无 AAD，恒为 0），后 8 字节是 IV 的比特数
        b2_bytes[8..16].copy_from_slice(&((iv.len() as u64) * 8).to_be_bytes());
        ghash.update(&[GenericArray::clone_from_slice(&b2_bytes)]);

        let mut out_j0 = [0u8; 16];
        let tag_val = ghash.finalize();
        out_j0.copy_from_slice(tag_val.as_slice());
        out_j0
    };

    // 2. 校验 GCM 认证 tag：tag = GHASH_H(A || C || [len(A)]_64 || [len(C)]_64) XOR E_K(J0)
    //    这里 AAD 为空，所以只有 C 参与。必须在 CTR 解密**之前**用原始密文计算，
    //    因为下面的 apply_keystream 是原地改写 ciphertext 的。
    if tag.len() != 16 {
        return Err(format!(
            "Invalid Factory credential format; authTag must be 16 bytes, got {}",
            tag.len()
        ));
    }

    let mut tag_ghash = GHash::new(GenericArray::from_slice(&h));
    tag_ghash.update_padded(ciphertext); // C，末尾不足一块自动补零
    let mut len_block = [0u8; 16];
    // 前 8 字节是 len(A) 的比特数（AAD 为空，恒为 0），后 8 字节是 len(C) 的比特数，
    // 都必须是完整的 64 位大端表示
    len_block[8..16].copy_from_slice(&((ciphertext.len() as u64) * 8).to_be_bytes());
    tag_ghash.update(&[GenericArray::clone_from_slice(&len_block)]);

    let mut ek_j0 = GenericArray::clone_from_slice(&j0);
    cipher_block.encrypt_block(&mut ek_j0);

    // T = GHASH_H(C || 0^u || [len(A)]_64 || [len(C)]_64) XOR E_K(J0)
    // 少了这一步 XOR 就是拿原始 GHASH 结果去比 tag，永远校验不过
    let mut computed_tag = tag_ghash.finalize();
    for i in 0..16 {
        computed_tag[i] ^= ek_j0[i];
    }

    // 常量时间比较：逐字节累积差异，避免 == 的提前退出泄露信息
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= computed_tag[i] ^ tag[i];
    }
    if diff != 0 {
        return Err("cred.decrypt_key_mismatch".into());
    }

    // 3. J1 = J0 + 1 (最后4字节大端自增) 作为 CTR 解密的初始块
    let mut j1 = j0;
    let ctr_num = u32::from_be_bytes(j0[12..16].try_into().unwrap());
    j1[12..16].copy_from_slice(&(ctr_num.wrapping_add(1)).to_be_bytes());

    // 4. 用 J1 作为初始计数器做 CTR 解密（原地改写密文的副本，原始密文已在上面喂给 GHASH）
    let mut plaintext = ciphertext.to_vec();
    let mut ctr_cipher = Aes256Ctr32BE::new(key_bytes.into(), (&j1).into());
    ctr_cipher.apply_keystream(&mut plaintext);

    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::prelude::*;

    /// 测试用密钥与明文。密钥是 0x00..0x1f 的固定序列，明文形如真实的
    /// Droid 登录态 JSON，方便断言解密结果本身而不只是「没报错」。
    const PLAINTEXT: &str =
        r#"{"access_token":"test-token-abc123","active_organization_id":"org_test_42"}"#;

    /// 0x00..0x1f：与生成测试向量时 Node 侧用的 `000102...1f` 是同一把钥匙。
    fn key() -> Vec<u8> {
        (0..32u8).collect()
    }

    fn decode(s: &str) -> Vec<u8> {
        BASE64_STANDARD.decode(s.as_bytes()).unwrap()
    }

    /// 这些向量由 Node 的 `crypto.createCipheriv('aes-256-gcm', ...)` 生成，
    /// 生成脚本在仓库的 `scripts/gen-gcm-vectors.cjs`（改密钥/明文后可重跑）。
    ///
    /// **为什么必须用 Node 生成的向量**：Droid CLI 是 Node 写的，它的 IV 是 16 字节，
    /// 而 `aes-gcm` crate 只支持 12 字节 nonce——这正是这里手搓 GCM 的原因。
    /// 拿 Node 真实产出的密文来喂 `decrypt_factory_payload`，才能证明我们的
    /// J0 推导、tag 校验、CTR 计数器和写出方一致，而不是自己和自己对得上。
    #[test]
    fn decrypts_node_openssl_vector_with_16_byte_iv() {
        let iv = decode("AQIDBAUGBwgJCgsMDQ4PEA==");
        let tag = decode("2IRK7xud3vnzIqqV+C2psg==");
        let ct = decode(
            "nKf1tb4pHdnOo1AteNoeKgArGDg4EpahJYCnk0N2iocI1Mpd813L5bkKew5u19voZt8ICMLZdGHXOjpv7Bp+8TqcLcvhYWQ8+PyV",
        );

        let out = decrypt_factory_payload(&key(), &iv, &tag, &ct).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), PLAINTEXT);
    }

    /// 12 字节 IV 走 NIST 的 J0 = IV || 0^31 || 1 分支，是另一条代码路径。
    #[test]
    fn decrypts_node_openssl_vector_with_12_byte_iv() {
        let iv = decode("AQIDBAUGBwgJCgsM");
        let tag = decode("fstVQNFMf2Z6ADQSCckyRg==");
        let ct = decode("fsg7to/xg/UT1gwsdX3IEmA3hI3jQCSqyjqXCf/UFfS217nzTNSP69na37qqqZ888J81gaDwWR9pFGjvVSNbfhAif0bMhQLUeTDN");

        let out = decrypt_factory_payload(&key(), &iv, &tag, &ct).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), PLAINTEXT);
    }

    /// 32 字节 IV 也必须能解（走 GHASH 推导 J0 的分支，且补位长度是 256 而不是 128）。
    #[test]
    fn decrypts_node_openssl_vector_with_32_byte_iv() {
        let iv = decode("AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=");
        let tag = decode("k3mXhRxDgbPlLMqaX0CT4g==");
        let ct = decode("qN9DfY/i3e7K9MTdoLb456jAowzBYuX30j0RIUgOGB/h9muo4uNJTpFodwiMJsddK+Mbogka6X1bJm+lvpTYER8BMEjUHbn3GMJ7");

        let out = decrypt_factory_payload(&key(), &iv, &tag, &ct).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), PLAINTEXT);
    }

    /// 空明文：密文长度为 0，tag 仍然必须校验通过（易漏的边界）。
    #[test]
    fn decrypts_empty_plaintext() {
        let iv = decode("AQIDBAUGBwgJCgsMDQ4PEA==");
        let tag = decode("lTLod13rv968aNf+3/LCJg==");

        let out = decrypt_factory_payload(&key(), &iv, &tag, &[]).unwrap();
        assert!(out.is_empty());
    }

    /// 认证失败必须报错，绝不能把垃圾解出来当凭据用。
    /// 这里改 tag 的第一个字节，模拟密钥不匹配 / 文件被篡改。
    #[test]
    fn wrong_tag_is_rejected() {
        let iv = decode("AQIDBAUGBwgJCgsMDQ4PEA==");
        let mut tag = decode("2IRK7xud3vnzIqqV+C2psg==");
        tag[0] ^= 0x01;
        let ct = decode(
            "nKf1tb4pHdnOo1AteNoeKgArGDg4EpahJYCnk0N2iocI1Mpd813L5bkKew5u19voZt8ICMLZdGHXOjpv7Bp+8TqcLcvhYWQ8+PyV",
        );

        let err = decrypt_factory_payload(&key(), &iv, &tag, &ct).unwrap_err();
        assert_eq!(err, "cred.decrypt_key_mismatch");
    }

    /// 用错误的密钥解同一份密文也必须被 tag 校验拦下。
    #[test]
    fn wrong_key_is_rejected() {
        let iv = decode("AQIDBAUGBwgJCgsMDQ4PEA==");
        let tag = decode("2IRK7xud3vnzIqqV+C2psg==");
        let ct = decode(
            "nKf1tb4pHdnOo1AteNoeKgArGDg4EpahJYCnk0N2iocI1Mpd813L5bkKew5u19voZt8ICMLZdGHXOjpv7Bp+8TqcLcvhYWQ8+PyV",
        );

        let mut bad_key = key();
        bad_key[31] ^= 0xff;

        assert_eq!(
            decrypt_factory_payload(&bad_key, &iv, &tag, &ct).unwrap_err(),
            "cred.decrypt_key_mismatch"
        );
    }

    /// 密文被改动同样必须报错（GCM 的完整性保证，不只是密钥正确性）。
    #[test]
    fn tampered_ciphertext_is_rejected() {
        let iv = decode("AQIDBAUGBwgJCgsMDQ4PEA==");
        let tag = decode("2IRK7xud3vnzIqqV+C2psg==");
        let mut ct = decode(
            "nKf1tb4pHdnOo1AteNoeKgArGDg4EpahJYCnk0N2iocI1Mpd813L5bkKew5u19voZt8ICMLZdGHXOjpv7Bp+8TqcLcvhYWQ8+PyV",
        );
        ct[0] ^= 0x80;

        assert_eq!(
            decrypt_factory_payload(&key(), &iv, &tag, &ct).unwrap_err(),
            "cred.decrypt_key_mismatch"
        );
    }

    /// tag 长度不是 16 字节时要报「格式非法」而不是静默接受，
    /// 否则短 tag 会被当成前缀比较而降低认证强度。
    #[test]
    fn short_tag_is_a_format_error() {
        let iv = decode("AQIDBAUGBwgJCgsMDQ4PEA==");
        let tag = vec![0u8; 8];

        let err = decrypt_factory_payload(&key(), &iv, &tag, b"x").unwrap_err();
        assert!(err.contains("authTag must be 16 bytes"), "got: {err}");
    }

    /// IV 长度既不是 12/16/32 也不足 16 时必须拒绝：
    /// 少于 16 字节无法凑满 GHASH 的第一个块，继续算就是读越界。
    #[test]
    fn unsupported_iv_length_is_rejected() {
        let short_iv = vec![0u8; 8];
        let tag = vec![0u8; 16];

        let err = decrypt_factory_payload(&key(), &short_iv, &tag, b"x").unwrap_err();
        assert!(err.contains("unsupported iv length"), "got: {err}");
    }

    /// key 长度不对时 `Aes256::new_from_slice` 必须报错而不是凑合。
    #[test]
    fn wrong_key_length_is_rejected() {
        let iv = decode("AQIDBAUGBwgJCgsMDQ4PEA==");
        let tag = vec![0u8; 16];

        assert!(decrypt_factory_payload(&[0u8; 16], &iv, &tag, b"x").is_err());
    }
}
