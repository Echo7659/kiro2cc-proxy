// Copyright (c) 2026 Harllan He. Licensed under MIT.
//! STORED（无压缩）ZIP 字节产出 helper。
//!
//! 仅实现 ZIP 规范中最简的 method 0（STORED）布局，以遵守「不引入新外部 crate」约定：
//!
//! ```text
//! [本地文件头 30B + 文件名] [文件数据]   × N
//! [中央目录记录 46B + 文件名]            × N
//! [EOCD 22B]
//! ```
//!
//! # 未实现 ZIP64（重要约束）
//! 中央目录偏移与 EOCD 字段均以 u32 表示，跨 4 GiB 会溢出；
//! EOCD 的条目计数字段为 u16，条目数超过 65535 同样会被截断。
//! 调用方必须保证归档总量小于 4 GiB（见 [`crate::common::dump::MAX_ARCHIVE_BYTES`]，
//! 该常量有编译期守卫断言其 < 4 GiB）。**调高上限前先评估 ZIP64 必要性。**
//!
//! 本模块只产出字节片段，不负责编排；批量与流式两条路径共用同一组 helper，
//! 避免两套 ZIP 逻辑各自演进而分叉。

use crate::kiro::parser::crc::crc32;

/// ZIP 2.0 —— STORED 所需的最低 version needed to extract
const VERSION_NEEDED: u16 = 20;

/// STORED 压缩方法编号（无压缩）
const METHOD_STORED: u16 = 0;

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;

/// 本地文件头长度（不含文件名）
pub const LOCAL_HEADER_LEN: usize = 30;
/// 中央目录记录长度（不含文件名）
pub const CENTRAL_ENTRY_LEN: usize = 46;
/// EOCD 长度
pub const EOCD_LEN: usize = 22;

/// 中央目录条目：记录某个已写入条目的元数据与其本地头偏移
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CentralEntry {
    pub name: String,
    pub crc: u32,
    pub size: u32,
    /// 该条目本地文件头在本归档中的字节偏移
    pub local_header_offset: u32,
}

/// 取纯文件名（拒绝路径分隔符），保证 ZIP 内条目名不含目录层级
fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// 计算条目数据的 CRC-32（ISO-HDLC，与 ZIP 规范一致）
pub fn entry_crc(data: &[u8]) -> u32 {
    crc32(data)
}

/// 收敛长度到 ZIP 的 u16 计数字段（文件名长度、条目总数）。
///
/// debug 构建下断言不溢出；release 下截断不含额外检查，
/// 依赖调用方满足「文件名单段名 ≤ 255 字节、条目数 ≤ [`crate::common::dump::MAX_DUMP_FILES`]」约束。
pub fn as_u16_len(len: usize) -> u16 {
    debug_assert!(len <= u16::MAX as usize, "ZIP u16 计数字段溢出: {len}");
    len as u16
}

/// 收敛长度到 ZIP 的 u32 字段（条目大小、偏移）。
///
/// debug 构建下断言不溢出（未实现 ZIP64 的前提，见模块头注释）。
pub fn as_u32_len(len: usize) -> u32 {
    debug_assert!(len <= u32::MAX as usize, "ZIP u32 字段溢出: {len}");
    len as u32
}

/// 构造本地文件头（30B + 文件名）
///
/// 字段：version needed = 20、gp flag = 0、method = STORED、
/// mod time/date = 0（避免 MS-DOS 时间转换；文件名已含毫秒时间戳）、
/// compressed size == uncompressed size == `size`。
pub fn local_header(name: &str, crc: u32, size: u32) -> Vec<u8> {
    let name_bytes = basename(name).as_bytes();
    let mut v = Vec::with_capacity(LOCAL_HEADER_LEN + name_bytes.len());
    push_u32(&mut v, SIG_LOCAL);
    push_u16(&mut v, VERSION_NEEDED);
    push_u16(&mut v, 0); // general purpose bit flag
    push_u16(&mut v, METHOD_STORED);
    push_u16(&mut v, 0); // last mod file time
    push_u16(&mut v, 0); // last mod file date
    push_u32(&mut v, crc);
    push_u32(&mut v, size); // compressed size
    push_u32(&mut v, size); // uncompressed size
    push_u16(&mut v, as_u16_len(name_bytes.len()));
    push_u16(&mut v, 0); // extra field length
    v.extend_from_slice(name_bytes);
    v
}

/// 构造中央目录记录（46B + 文件名）
///
/// `mod time/date = 0`、`internal/external attrs = 0`、`disk number = 0`，
/// `local_header_offset` 取条目本地头在归档中的偏移。
pub fn central_entry(entry: &CentralEntry) -> Vec<u8> {
    let name_bytes = basename(&entry.name).as_bytes();
    let mut v = Vec::with_capacity(CENTRAL_ENTRY_LEN + name_bytes.len());
    push_u32(&mut v, SIG_CENTRAL);
    push_u16(&mut v, VERSION_NEEDED); // version made by
    push_u16(&mut v, VERSION_NEEDED); // version needed
    push_u16(&mut v, 0); // gp flag
    push_u16(&mut v, METHOD_STORED);
    push_u16(&mut v, 0); // mod time
    push_u16(&mut v, 0); // mod date
    push_u32(&mut v, entry.crc);
    push_u32(&mut v, entry.size); // compressed size
    push_u32(&mut v, entry.size); // uncompressed size
    push_u16(&mut v, as_u16_len(name_bytes.len()));
    push_u16(&mut v, 0); // extra field length
    push_u16(&mut v, 0); // file comment length
    push_u16(&mut v, 0); // disk number start
    push_u16(&mut v, 0); // internal file attributes
    push_u32(&mut v, 0); // external file attributes
    push_u32(&mut v, entry.local_header_offset);
    v.extend_from_slice(name_bytes);
    v
}

/// 构造 EOCD（22B）
///
/// `count` 为总条目数（含可能追加的说明文件），`cd_size` 为中央目录总字节数，
/// `cd_offset` 为中央目录起始偏移。`count` 经 [`as_u16_len`] 收敛（未实现 ZIP64）。
pub fn eocd(count: usize, cd_size: u32, cd_offset: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(EOCD_LEN);
    push_u32(&mut v, SIG_EOCD);
    push_u16(&mut v, 0); // number of this disk
    push_u16(&mut v, 0); // disk with central directory
    push_u16(&mut v, as_u16_len(count)); // entries on this disk
    push_u16(&mut v, as_u16_len(count)); // total entries
    push_u32(&mut v, cd_size);
    push_u32(&mut v, cd_offset);
    push_u16(&mut v, 0); // comment length
    v
}

fn push_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// 内存批量构造完整归档（仅测试使用）。
///
/// 生产路径只有 `dump_handler` 的流式产出；本函数存在的唯一目的是让单测能把
/// 流式结果与「一次性构造」的结果逐字节比对，从而验证两者共用同一组 helper。
#[cfg(test)]
pub fn build_archive(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let crc = entry_crc(data);
        let offset = as_u32_len(out.len());
        let size = as_u32_len(data.len());
        out.extend_from_slice(&local_header(name, crc, size));
        out.extend_from_slice(data);
        central.extend_from_slice(&central_entry(&CentralEntry {
            name: name.clone(),
            crc,
            size,
            local_header_offset: offset,
        }));
    }
    let cd_offset = as_u32_len(out.len());
    let cd_size = as_u32_len(central.len());
    out.extend_from_slice(&central);
    out.extend_from_slice(&eocd(entries.len(), cd_size, cd_offset));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u16(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }

    fn read_u32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn test_local_header_layout() {
        let data = b"hello world";
        let crc = entry_crc(data);
        let h = local_header("body-1.json", crc, data.len() as u32);

        assert_eq!(h.len(), LOCAL_HEADER_LEN + "body-1.json".len());
        assert_eq!(read_u32(&h, 0), SIG_LOCAL);
        assert_eq!(read_u16(&h, 4), VERSION_NEEDED);
        assert_eq!(read_u16(&h, 6), 0, "gp flag 应为 0");
        assert_eq!(read_u16(&h, 8), METHOD_STORED);
        assert_eq!(read_u16(&h, 10), 0, "mod time 应为 0");
        assert_eq!(read_u16(&h, 12), 0, "mod date 应为 0");
        assert_eq!(read_u32(&h, 14), crc, "CRC 应等于数据实际 CRC");
        assert_eq!(read_u32(&h, 18), data.len() as u32, "compressed size");
        assert_eq!(read_u32(&h, 22), data.len() as u32, "uncompressed size");
        assert_eq!(read_u16(&h, 26), "body-1.json".len() as u16);
        assert_eq!(read_u16(&h, 28), 0, "extra len 应为 0");
        assert_eq!(&h[30..], b"body-1.json");
    }

    #[test]
    fn test_local_header_rejects_path_in_entry_name() {
        // 条目名只取 basename，避免 ZIP 内出现目录层级
        let h = local_header("sub/body-1.json", 0, 0);
        assert_eq!(&h[LOCAL_HEADER_LEN..], b"body-1.json");
    }

    #[test]
    fn test_central_entry_layout() {
        let data = b"abc";
        let crc = entry_crc(data);
        let e = CentralEntry {
            name: "body-7.json".into(),
            crc,
            size: data.len() as u32,
            local_header_offset: 1234,
        };
        let c = central_entry(&e);

        assert_eq!(c.len(), CENTRAL_ENTRY_LEN + "body-7.json".len());
        assert_eq!(read_u32(&c, 0), SIG_CENTRAL);
        assert_eq!(read_u16(&c, 4), VERSION_NEEDED);
        assert_eq!(read_u16(&c, 6), VERSION_NEEDED);
        assert_eq!(read_u16(&c, 10), METHOD_STORED);
        assert_eq!(read_u32(&c, 16), crc);
        assert_eq!(read_u32(&c, 20), data.len() as u32);
        assert_eq!(read_u32(&c, 24), data.len() as u32);
        assert_eq!(read_u16(&c, 28), "body-7.json".len() as u16);
        assert_eq!(read_u32(&c, 42), 1234, "local_header_offset");
        assert_eq!(&c[CENTRAL_ENTRY_LEN..], b"body-7.json");
    }

    #[test]
    fn test_eocd_layout() {
        let e = eocd(3, 999, 555);
        assert_eq!(e.len(), EOCD_LEN);
        assert_eq!(read_u32(&e, 0), SIG_EOCD);
        assert_eq!(read_u16(&e, 4), 0);
        assert_eq!(read_u16(&e, 6), 0);
        assert_eq!(read_u16(&e, 8), 3, "entries on this disk");
        assert_eq!(read_u16(&e, 10), 3, "total entries");
        assert_eq!(read_u32(&e, 12), 999, "central dir size");
        assert_eq!(read_u32(&e, 16), 555, "central dir offset");
        assert_eq!(read_u16(&e, 20), 0);
    }

    /// 不依赖实现自身的中间变量：从产物反推 EOCD 字段并与「手工按规范重算」的值比对
    #[test]
    fn test_empty_archive_eocd_fields() {
        let archive = build_archive(&[]);
        assert_eq!(archive.len(), EOCD_LEN, "空归档只含 EOCD");

        let cd_offset = read_u32(&archive, 16);
        let cd_size = read_u32(&archive, 12);
        assert_eq!(cd_offset, 0, "空归档中央目录起始于 0");
        assert_eq!(cd_size, 0, "空归档中央目录为空");
        assert_eq!(read_u16(&archive, 10), 0, "total entries 为 0");
    }

    #[test]
    fn test_single_file_archive_offsets_recomputed_manually() {
        let data = b"payload-for-single-file";
        let archive = build_archive(&[("body-1.json".to_string(), data.to_vec())]);

        // 手工按规范重算期望偏移，不使用实现产出的中间变量
        let name_len = "body-1.json".len();
        let expected_cd_offset = LOCAL_HEADER_LEN + name_len + data.len();
        let expected_cd_size = CENTRAL_ENTRY_LEN + name_len;

        // 手工重算的中央目录起始位置应正好是中央目录记录的 signature
        assert_eq!(read_u32(&archive, expected_cd_offset), SIG_CENTRAL);

        let eocd_at = archive.len() - EOCD_LEN;
        assert_eq!(read_u16(&archive, eocd_at + 10), 1, "total entries = 1");
        assert_eq!(
            read_u32(&archive, eocd_at + 12) as usize,
            expected_cd_size,
            "central dir size 应为 46 + 文件名长度"
        );
        assert_eq!(
            read_u32(&archive, eocd_at + 16) as usize,
            expected_cd_offset,
            "central dir offset 应为本地头 + 名 + 数据总长"
        );
    }

    #[test]
    fn test_multi_file_archive_roundtrip_bytes() {
        let entries = vec![
            ("body-1.json".to_string(), b"aaa".to_vec()),
            ("body-2.json".to_string(), b"bbbb".to_vec()),
            ("body-3.json".to_string(), b"c".to_vec()),
        ];
        let archive = build_archive(&entries);

        // 逐个条目按本地头遍历，校验 CRC / 长度 / 数据逐字节一致
        let mut pos = 0usize;
        for (name, expected) in &entries {
            assert_eq!(read_u32(&archive, pos), SIG_LOCAL);
            let n = read_u16(&archive, pos + 26) as usize;
            assert_eq!(&archive[pos + 30..pos + 30 + n], name.as_bytes());
            let crc = read_u32(&archive, pos + 14);
            let size = read_u32(&archive, pos + 18) as usize;
            assert_eq!(size, expected.len());
            assert_eq!(crc, entry_crc(expected));
            let data_at = pos + LOCAL_HEADER_LEN + n;
            assert_eq!(&archive[data_at..data_at + size], expected.as_slice());
            pos = data_at + size;
        }

        // 中央目录紧跟数据区，条目数与偏移自洽
        let eocd_at = archive.len() - EOCD_LEN;
        assert_eq!(read_u16(&archive, eocd_at + 10), 3, "total entries = 3");
        assert_eq!(read_u32(&archive, eocd_at + 16) as usize, pos);
        assert_eq!(read_u32(&archive, pos), SIG_CENTRAL);
    }

    #[test]
    fn test_entry_crc_matches_kiro_crc() {
        // 与 src/kiro/parser/crc.rs 同一算法（ISO-HDLC），ZIP 规范要求的正是它
        assert_eq!(entry_crc(b""), crc32(b""));
        assert_eq!(entry_crc(b"123456789"), 0xCBF43926);
    }
}
