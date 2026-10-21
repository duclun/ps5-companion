//! Bounded recognition of unsigned PS5 PFS containers holding PFSC images.
//! Layout reference: PSBrew/MkPFS, parse_image_header/parse_image_inode and
//! _parse_pfsc_header. This validates framing, not game authenticity/playability.
use crate::error::{CompanionError, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

fn read_at(file: &mut File, offset: u64, bytes: &mut [u8]) -> Result<bool> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| CompanionError::io("seek PFS container", error))?;
    match file.read_exact(bytes) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(CompanionError::io("read PFS container", error)),
    }
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

pub(crate) fn is_nested_pfsc_image(path: &Path) -> Result<bool> {
    let mut file =
        File::open(path).map_err(|error| CompanionError::io("open PFS container", error))?;
    let size = file
        .metadata()
        .map_err(|error| CompanionError::io("stat PFS container", error))?
        .len();
    let mut header = [0u8; 0x400];
    if !read_at(&mut file, 0, &mut header)? {
        return Ok(false);
    }
    let block = u32_at(&header, 0x20) as u64;
    let count = u64_at(&header, 0x30);
    let inode_blocks = u64_at(&header, 0x40);
    let mode = u16::from_le_bytes(header[0x1c..0x1e].try_into().unwrap());
    if u64_at(&header, 0) != 2
        || u64_at(&header, 8) != 20_130_315
        || mode & 7 != 0
        || !(4096..=65536).contains(&block)
        || !block.is_power_of_two()
        || count == 0
        || count > 16384
        || inode_blocks == 0
        || inode_blocks > 1024
        || u64_at(&header, 0x38).checked_mul(block) != Some(size)
        || inode_blocks
            .checked_mul(block)
            .and_then(|v| v.checked_add(block))
            .map_or(true, |v| v > size)
        || count > inode_blocks * (block / 0xa8)
    {
        return Ok(false);
    }
    let mut inode = [0u8; 0xa8];
    for index in 0..count {
        let offset = block + (index / (block / 0xa8)) * block + (index % (block / 0xa8)) * 0xa8;
        if !read_at(&mut file, offset, &mut inode)? {
            return Ok(false);
        }
        let kind = u16::from_le_bytes(inode[0..2].try_into().unwrap());
        if kind & 0xf000 != 0x8000 || u32_at(&inode, 4) & 1 == 0 {
            continue;
        }
        let stored = u64_at(&inode, 8);
        let logical = u64_at(&inode, 16);
        let start = u32_at(&inode, 0x64) as u64 * block;
        if start < (inode_blocks + 1) * block
            || stored < 0x10001
            || start.checked_add(stored).map_or(true, |end| end > size)
        {
            continue;
        }
        let mut pfsc = [0u8; 0x30];
        if !read_at(&mut file, start, &mut pfsc)? {
            continue;
        }
        if &pfsc[..4] != b"PFSC"
            || u32_at(&pfsc, 4) != 0
            || u32_at(&pfsc, 8) != 6
            || u32_at(&pfsc, 12) != 65536
            || u64_at(&pfsc, 16) != 65536
            || u64_at(&pfsc, 24) != 0x400
            || logical == 0
            || logical % 65536 != 0
            || u64_at(&pfsc, 40) != logical
        {
            continue;
        }
        let table_end = (logical / 65536 + 1)
            .checked_mul(8)
            .and_then(|v| v.checked_add(0x400));
        let data = u64_at(&pfsc, 32);
        if data < 65536 || data >= stored || table_end.map_or(true, |end| end > data) {
            continue;
        }
        let mut first = [0u8; 8];
        let mut last = [0u8; 8];
        if !read_at(&mut file, start + 0x400, &mut first)?
            || !read_at(&mut file, start + 0x400 + logical / 65536 * 8, &mut last)?
        {
            continue;
        }
        if u64::from_le_bytes(first) == data
            && u64::from_le_bytes(last) > data
            && u64::from_le_bytes(last) <= stored
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<u8> {
        let mut b = vec![0u8; 8 * 65536];
        for (offset, value) in [(0, 2u64), (8, 20_130_315), (0x30, 4), (0x38, 8), (0x40, 1)] {
            b[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        b[0x20..0x24].copy_from_slice(&65536u32.to_le_bytes());
        let i = 65536 + 3 * 0xa8;
        b[i..i + 2].copy_from_slice(&0x8000u16.to_le_bytes());
        b[i + 4..i + 8].copy_from_slice(&1u32.to_le_bytes());
        b[i + 8..i + 16].copy_from_slice(&65537u64.to_le_bytes());
        b[i + 16..i + 24].copy_from_slice(&65536u64.to_le_bytes());
        b[i + 0x64..i + 0x68].copy_from_slice(&6u32.to_le_bytes());
        let p = 6 * 65536;
        b[p..p + 4].copy_from_slice(b"PFSC");
        b[p + 8..p + 12].copy_from_slice(&6u32.to_le_bytes());
        b[p + 12..p + 16].copy_from_slice(&65536u32.to_le_bytes());
        for (offset, value) in [
            (16, 65536u64),
            (24, 0x400),
            (32, 65536),
            (40, 65536),
            (0x400, 65536),
            (0x408, 65537),
        ] {
            b[p + offset..p + offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        b
    }
    #[test]
    fn accepts_nested_container_and_rejects_bad_framing() {
        let path = std::env::temp_dir().join(format!("ps5-pfsc-header-{}.bin", std::process::id()));
        let good = fixture();
        std::fs::write(&path, &good).unwrap();
        assert!(is_nested_pfsc_image(&path).unwrap());
        for (offset, byte) in [
            (0, 1u8),
            (8, 0),
            (6 * 65536 + 8, 5),
            (6 * 65536 + 25, 0),
            (6 * 65536 + 40, 1),
        ] {
            let mut bad = good.clone();
            bad[offset] = byte;
            std::fs::write(&path, bad).unwrap();
            assert!(!is_nested_pfsc_image(&path).unwrap(), "bad offset {offset}");
        }
        std::fs::write(&path, &good[..1000]).unwrap();
        assert!(!is_nested_pfsc_image(&path).unwrap());
        std::fs::remove_file(path).unwrap();
    }
}
