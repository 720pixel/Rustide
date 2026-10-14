use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/rustide.ico");
    if env::var_os("CARGO_FEATURE_GUI").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
    {
        return;
    }
    let icon = fs::read("assets/rustide.ico").expect("read Rustide icon");
    let resource = icon_resource(&icon).expect("valid Rustide icon");
    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory")).join("rustide.res");
    fs::write(&output, resource).expect("write Rustide icon resource");
    println!("cargo:rustc-link-arg-bin=rustide={}", output.display());
}

fn icon_resource(icon: &[u8]) -> Result<Vec<u8>, &'static str> {
    if icon.get(..4) != Some(&[0, 0, 1, 0]) {
        return Err("invalid ICO header");
    }
    let count = word(icon, 4)?;
    if count == 0 {
        return Err("ICO has no images");
    }
    let mut output = Vec::new();
    resource(&mut output, 0, 0, &[]);
    let mut group = icon[..6].to_vec();
    for index in 0..usize::from(count) {
        let start = 6 + index * 16;
        let entry = icon
            .get(start..start + 16)
            .ok_or("truncated ICO directory")?;
        let length = dword(entry, 8)? as usize;
        let offset = dword(entry, 12)? as usize;
        let end = offset
            .checked_add(length)
            .ok_or("ICO image size overflow")?;
        let image = icon.get(offset..end).ok_or("truncated ICO image")?;
        let id = (index + 1) as u16;
        resource(&mut output, 3, id, image);
        group.extend_from_slice(&entry[..12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    // RT_GROUP_ICON #1 references the individual RT_ICON resources.
    resource(&mut output, 14, 1, &group);
    Ok(output)
}

fn resource(output: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_le_bytes());
    output.extend_from_slice(&32_u32.to_le_bytes());
    for ordinal in [kind, id] {
        output.extend_from_slice(&0xffff_u16.to_le_bytes());
        output.extend_from_slice(&ordinal.to_le_bytes());
    }
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&(if kind == 0 { 0_u16 } else { 0x1030_u16 }).to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(data);
    while !output.len().is_multiple_of(4) {
        output.push(0);
    }
}

fn word(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
    let bytes = bytes.get(offset..offset + 2).ok_or("truncated ICO field")?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn dword(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
    let bytes = bytes.get(offset..offset + 4).ok_or("truncated ICO field")?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_group_references_aligned_matching_images() {
        let icon = include_bytes!("assets/rustide.ico");
        let resource = icon_resource(icon).unwrap();
        let mut cursor = 32;
        let count = usize::from(word(icon, 4).unwrap());
        for index in 0..count {
            assert_eq!(cursor % 4, 0);
            assert_eq!(dword(&resource, cursor + 4).unwrap(), 32);
            assert_eq!(word(&resource, cursor + 10).unwrap(), 3);
            assert_eq!(word(&resource, cursor + 14).unwrap(), (index + 1) as u16);
            let size = dword(&resource, cursor).unwrap() as usize;
            let entry = 6 + index * 16;
            let offset = dword(icon, entry + 12).unwrap() as usize;
            assert_eq!(
                &resource[cursor + 32..cursor + 32 + size],
                &icon[offset..offset + size]
            );
            cursor += 32 + size.next_multiple_of(4);
        }
        assert_eq!(word(&resource, cursor + 10).unwrap(), 14);
        assert_eq!(word(&resource, cursor + 14).unwrap(), 1);
        let group = &resource[cursor + 32..cursor + 32 + 6 + count * 14];
        assert_eq!(&group[..6], &icon[..6]);
        for index in 0..count {
            assert_eq!(
                &group[6 + index * 14..18 + index * 14],
                &icon[6 + index * 16..18 + index * 16]
            );
            assert_eq!(word(group, 18 + index * 14).unwrap(), (index + 1) as u16);
        }
        assert_eq!(
            cursor + 32 + (6 + count * 14).next_multiple_of(4),
            resource.len()
        );
    }

    #[test]
    fn truncated_icon_images_are_rejected() {
        let icon = include_bytes!("assets/rustide.ico");
        assert!(icon_resource(&icon[..icon.len() - 1]).is_err());
        assert!(icon_resource(&[0, 0, 1, 0, 0, 0]).is_err());
    }
}
