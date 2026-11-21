//! 把岛仔图标作为 RT_GROUP_ICON #1 编进 exe，供 LoadImageW(hInst, 1, ...) 取用。
//! 优先调用 Windows SDK 的 rc.exe；精简构建环境没有 SDK 时直接生成等价格式的 .res。

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    let roots = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    let mut found: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let rc = dir.join("x64").join("rc.exe");
            if rc.exists() {
                found.push(rc);
            }
        }
    }
    found.sort();
    found.pop()
}

fn push16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn resource(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, language: u16, data: &[u8]) {
    pad4(out);
    push32(out, data.len() as u32);
    push32(out, 32);
    push16(out, 0xffff);
    push16(out, kind);
    push16(out, 0xffff);
    push16(out, id);
    push32(out, 0);
    push16(out, flags);
    push16(out, language);
    push32(out, 0);
    push32(out, 0);
    out.extend_from_slice(data);
    pad4(out);
}

struct VersionNode {
    key: String,
    value: Vec<u8>,
    value_length: u16,
    text: bool,
    children: Vec<VersionNode>,
}

impl VersionNode {
    fn container(key: &str, children: Vec<Self>) -> Self {
        Self {
            key: key.into(),
            value: Vec::new(),
            value_length: 0,
            text: true,
            children,
        }
    }

    fn text(key: &str, value: &str) -> Self {
        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        let mut bytes = Vec::with_capacity(wide.len() * 2);
        for word in &wide {
            push16(&mut bytes, *word);
        }
        Self {
            key: key.into(),
            value: bytes,
            value_length: wide.len() as u16,
            text: true,
            children: Vec::new(),
        }
    }

    fn binary(key: &str, value: Vec<u8>, children: Vec<Self>) -> Self {
        let value_length = value.len() as u16;
        Self {
            key: key.into(),
            value,
            value_length,
            text: false,
            children,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        push16(out, 0);
        push16(out, self.value_length);
        push16(out, u16::from(self.text));
        for word in self.key.encode_utf16().chain(std::iter::once(0)) {
            push16(out, word);
        }
        pad4(out);
        out.extend_from_slice(&self.value);
        for child in &self.children {
            pad4(out);
            child.encode(out);
        }
        let length = (out.len() - start) as u16;
        out[start..start + 2].copy_from_slice(&length.to_le_bytes());
    }
}

fn version_resource() -> Result<Vec<u8>, String> {
    let version = env::var("CARGO_PKG_VERSION").map_err(|e| e.to_string())?;
    let numbers: Vec<u16> = version
        .split('.')
        .map(|part| part.parse::<u16>().map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    if numbers.len() != 3 {
        return Err(format!("无法解析版本号 {version}"));
    }
    let mut fixed = Vec::with_capacity(52);
    for value in [
        0xfeef04bd,
        0x00010000,
        u32::from(numbers[0]) << 16 | u32::from(numbers[1]),
        u32::from(numbers[2]) << 16,
        u32::from(numbers[0]) << 16 | u32::from(numbers[1]),
        u32::from(numbers[2]) << 16,
        0x3f,
        0,
        0x40004,
        1,
        0,
        0,
        0,
    ] {
        push32(&mut fixed, value);
    }
    let strings = VersionNode::container(
        "StringFileInfo",
        vec![VersionNode::container(
            "080404b0",
            [
                ("FileDescription", "岛仔".to_string()),
                ("InternalName", "Daozai".into()),
                ("OriginalFilename", "daozai.exe".into()),
                ("ProductName", "岛仔 Daozai".into()),
                ("ProductVersion", version.clone()),
                ("FileVersion", version),
            ]
            .into_iter()
            .map(|(key, value)| VersionNode::text(key, &value))
            .collect(),
        )],
    );
    let mut translation = Vec::new();
    push16(&mut translation, 0x0804);
    push16(&mut translation, 1200);
    let vars = VersionNode::container(
        "VarFileInfo",
        vec![VersionNode::binary("Translation", translation, Vec::new())],
    );
    let mut out = Vec::new();
    VersionNode::binary("VS_VERSION_INFO", fixed, vec![strings, vars]).encode(&mut out);
    Ok(out)
}

fn fallback_res() -> Result<Vec<u8>, String> {
    let ico = fs::read("assets/daozai.ico").map_err(|e| e.to_string())?;
    if ico.len() < 6 || ico[..4] != [0, 0, 1, 0] {
        return Err("assets/daozai.ico 不是有效图标".into());
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    if count == 0 || ico.len() < 6 + count * 16 {
        return Err("图标目录不完整".into());
    }
    let mut out = Vec::new();
    resource(&mut out, 0, 0, 0, 0, &[]);
    let mut group = ico[..6].to_vec();
    for index in 0..count {
        let entry = 6 + index * 16;
        let size = u32::from_le_bytes(ico[entry + 8..entry + 12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(ico[entry + 12..entry + 16].try_into().unwrap()) as usize;
        let image = ico.get(offset..offset + size).ok_or("图标图像数据不完整")?;
        let id = index as u16 + 1;
        resource(&mut out, 3, id, 0x1030, 0x0804, image);
        group.extend_from_slice(&ico[entry..entry + 8]);
        push32(&mut group, size as u32);
        push16(&mut group, id);
    }
    resource(&mut out, 14, 1, 0x1030, 0x0804, &group);
    resource(&mut out, 16, 1, 0x0030, 0x0804, &version_resource()?);
    Ok(out)
}

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/daozai.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("daozai.res");
    if let Some(rc) = find_rc() {
        let status = Command::new(&rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&out)
            .arg("app.rc")
            .status();
        if status.map(|s| s.success()).unwrap_or(false) {
            println!("cargo:rustc-link-arg={}", out.display());
            return;
        }
        println!("cargo:warning=rc.exe 编译 app.rc 失败，改用内置资源生成");
    }
    let bytes = fallback_res().unwrap_or_else(|error| panic!("生成图标资源失败：{error}"));
    fs::write(&out, bytes).unwrap();
    println!("cargo:rustc-link-arg={}", out.display());
}
