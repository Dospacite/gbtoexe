//! The payload that `gbtoexe` appends to the runtime stub.
//!
//! PE and ELF images both ignore bytes past the end of the last section, so the
//! converted game can simply be stapled to the tail of a finished executable.
//! The footer goes last so the runtime can find it by seeking backwards from EOF.
//!
//! ```text
//! [ runtime stub ][ rom ][ settings ][ translated code ][ block map ][ footer ]
//! ```
//!
//! The translated code is the real payload: native x86-64 produced by the
//! recompiler. It contains no absolute addresses, so it is valid wherever the
//! loader puts it, and the block map says which Game Boy address each entry
//! point in it corresponds to.

use std::io;

pub const MAGIC: &[u8; 8] = b"GBTOEXE1";
pub const FOOTER_LEN: usize = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Auto,
    Dmg,
    Cgb,
}

impl Model {
    fn to_byte(self) -> u8 {
        match self {
            Model::Auto => 0,
            Model::Dmg => 1,
            Model::Cgb => 2,
        }
    }

    fn from_byte(b: u8) -> Self {
        match b {
            1 => Model::Dmg,
            2 => Model::Cgb,
            _ => Model::Auto,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// Window title; defaults to the cartridge header title.
    pub title: String,
    /// Base name for the battery save file, without an extension.
    pub save_name: String,
    pub model: Model,
    /// Shades for monochrome games, lightest first, as 0xRRGGBB.
    pub palette: [u32; 4],
    pub scale: u8,
    pub fullscreen: bool,
    pub audio: bool,
    /// 0-100.
    pub volume: u8,
    pub sample_rate: u32,
    /// Keep the 10:9 aspect ratio when the window is resized.
    pub keep_aspect: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            title: "Game Boy".into(),
            save_name: "game".into(),
            model: Model::Auto,
            palette: [0xffffff, 0xaaaaaa, 0x555555, 0x000000],
            scale: 4,
            fullscreen: false,
            audio: true,
            volume: 70,
            sample_rate: 48_000,
            keep_aspect: true,
        }
    }
}

impl Settings {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(1u8); // settings format version
        out.push(self.model.to_byte());
        for c in self.palette {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out.push(self.scale);
        out.push(self.fullscreen as u8);
        out.push(self.audio as u8);
        out.push(self.volume);
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.push(self.keep_aspect as u8);
        write_string(&mut out, &self.title);
        write_string(&mut out, &self.save_name);
        out
    }

    pub fn decode(data: &[u8]) -> Option<Settings> {
        let mut r = Reader { data, pos: 0 };
        if r.u8()? != 1 {
            return None;
        }
        let model = Model::from_byte(r.u8()?);
        let mut palette = [0u32; 4];
        for slot in &mut palette {
            *slot = r.u32()?;
        }
        let scale = r.u8()?;
        let fullscreen = r.u8()? != 0;
        let audio = r.u8()? != 0;
        let volume = r.u8()?;
        let sample_rate = r.u32()?;
        let keep_aspect = r.u8()? != 0;
        let title = r.string()?;
        let save_name = r.string()?;

        Some(Settings {
            title,
            save_name,
            model,
            palette,
            scale,
            fullscreen,
            audio,
            volume,
            sample_rate,
            keep_aspect,
        })
    }
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(u16::MAX as usize);
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&bytes[..len]);
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let slice = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<String> {
        let len = u16::from_le_bytes(self.take(2)?.try_into().ok()?) as usize;
        Some(String::from_utf8_lossy(self.take(len)?).into_owned())
    }
}

/// One translated basic block's place in the code image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockEntry {
    /// ROM bank this block was translated for; 0 for the fixed window.
    pub bank: u16,
    /// Game Boy address the block starts at.
    pub addr: u16,
    /// Byte offset of its entry point within the code image.
    pub offset: u32,
    /// One past the last Game Boy address it covers.
    pub end: u16,
}

pub const BLOCK_ENTRY_LEN: usize = 10;

pub struct Payload {
    pub rom: Vec<u8>,
    pub settings: Settings,
    /// Native x86-64 for everything the converter could resolve ahead of time.
    pub code: Vec<u8>,
    pub blocks: Vec<BlockEntry>,
}

/// Build the bytes to append to a stub executable.
pub fn build(rom: &[u8], settings: &Settings, code: &[u8], blocks: &[BlockEntry]) -> Vec<u8> {
    let encoded = settings.encode();
    let mut body =
        Vec::with_capacity(rom.len() + encoded.len() + code.len() + blocks.len() * BLOCK_ENTRY_LEN);
    body.extend_from_slice(rom);
    body.extend_from_slice(&encoded);
    body.extend_from_slice(code);
    for block in blocks {
        body.extend_from_slice(&block.bank.to_le_bytes());
        body.extend_from_slice(&block.addr.to_le_bytes());
        body.extend_from_slice(&block.offset.to_le_bytes());
        body.extend_from_slice(&block.end.to_le_bytes());
    }

    let checksum = crc32(&body);
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&(rom.len() as u32).to_le_bytes());
    body.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
    body.extend_from_slice(&(code.len() as u32).to_le_bytes());
    body.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
    body.extend_from_slice(&checksum.to_le_bytes());
    body
}

#[derive(Debug)]
pub enum ReadError {
    /// No payload footer; this is a bare stub, not a converted game.
    NotPresent,
    Corrupt(&'static str),
    Io(io::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::NotPresent => f.write_str("no embedded ROM"),
            ReadError::Corrupt(what) => write!(f, "embedded ROM is damaged: {what}"),
            ReadError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// Pull the payload out of a whole executable image held in memory.
pub fn extract(image: &[u8]) -> Result<Payload, ReadError> {
    if image.len() < FOOTER_LEN {
        return Err(ReadError::NotPresent);
    }
    let footer = &image[image.len() - FOOTER_LEN..];
    if &footer[..8] != MAGIC {
        return Err(ReadError::NotPresent);
    }

    let rom_len = u32::from_le_bytes(footer[8..12].try_into().unwrap()) as usize;
    let settings_len = u32::from_le_bytes(footer[12..16].try_into().unwrap()) as usize;
    let code_len = u32::from_le_bytes(footer[16..20].try_into().unwrap()) as usize;
    let block_count = u32::from_le_bytes(footer[20..24].try_into().unwrap()) as usize;
    let expected_crc = u32::from_le_bytes(footer[24..28].try_into().unwrap());

    let body_len = rom_len
        .checked_add(settings_len)
        .and_then(|n| n.checked_add(code_len))
        .and_then(|n| {
            block_count
                .checked_mul(BLOCK_ENTRY_LEN)
                .and_then(|b| n.checked_add(b))
        })
        .ok_or(ReadError::Corrupt("implausible length"))?;
    if body_len + FOOTER_LEN > image.len() {
        return Err(ReadError::Corrupt("truncated file"));
    }

    let start = image.len() - FOOTER_LEN - body_len;
    let body = &image[start..start + body_len];
    if crc32(body) != expected_crc {
        return Err(ReadError::Corrupt("checksum mismatch"));
    }

    let rom = body[..rom_len].to_vec();
    let settings = Settings::decode(&body[rom_len..rom_len + settings_len])
        .ok_or(ReadError::Corrupt("unreadable settings"))?;
    let code_start = rom_len + settings_len;
    let code = body[code_start..code_start + code_len].to_vec();

    let mut blocks = Vec::with_capacity(block_count);
    let mut cursor = code_start + code_len;
    for _ in 0..block_count {
        let raw = &body[cursor..cursor + BLOCK_ENTRY_LEN];
        blocks.push(BlockEntry {
            bank: u16::from_le_bytes(raw[0..2].try_into().unwrap()),
            addr: u16::from_le_bytes(raw[2..4].try_into().unwrap()),
            offset: u32::from_le_bytes(raw[4..8].try_into().unwrap()),
            end: u16::from_le_bytes(raw[8..10].try_into().unwrap()),
        });
        cursor += BLOCK_ENTRY_LEN;
    }

    Ok(Payload {
        rom,
        settings,
        code,
        blocks,
    })
}

/// Read the payload appended to the currently running executable.
pub fn extract_from_self() -> Result<Payload, ReadError> {
    let path = std::env::current_exe().map_err(ReadError::Io)?;
    let image = std::fs::read(path).map_err(ReadError::Io)?;
    extract(&image)
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip() {
        let settings = Settings {
            title: "POKEMON RED".into(),
            scale: 3,
            model: Model::Cgb,
            palette: [1, 2, 3, 4],
            ..Settings::default()
        };

        let decoded = Settings::decode(&settings.encode()).unwrap();
        assert_eq!(decoded.title, "POKEMON RED");
        assert_eq!(decoded.scale, 3);
        assert_eq!(decoded.model, Model::Cgb);
        assert_eq!(decoded.palette, [1, 2, 3, 4]);
    }

    #[test]
    fn payload_is_found_at_the_end_of_a_stub() {
        let rom = vec![0xab; 1024];
        let code = vec![0x90; 64];
        let blocks = vec![BlockEntry {
            bank: 3,
            addr: 0x4123,
            offset: 16,
            end: 0x4130,
        }];
        let mut image = b"pretend this is a PE file".to_vec();
        image.extend_from_slice(&build(&rom, &Settings::default(), &code, &blocks));

        let payload = extract(&image).unwrap();
        assert_eq!(payload.rom, rom);
        assert_eq!(payload.settings.scale, 4);
        assert_eq!(payload.code, code);
        assert_eq!(payload.blocks, blocks);
    }

    #[test]
    fn a_bare_stub_reports_no_payload() {
        assert!(matches!(
            extract(b"just an executable"),
            Err(ReadError::NotPresent)
        ));
    }

    #[test]
    fn a_flipped_bit_is_caught() {
        let rom = vec![0x11; 64];
        let mut image = b"stub".to_vec();
        image.extend_from_slice(&build(&rom, &Settings::default(), &[], &[]));
        image[10] ^= 0xff;
        assert!(matches!(extract(&image), Err(ReadError::Corrupt(_))));
    }

    #[test]
    fn damaged_translated_code_is_caught_too() {
        // Native code that has been altered would be catastrophic to execute.
        let mut image = b"stub".to_vec();
        image.extend_from_slice(&build(&[0u8; 32], &Settings::default(), &[0x90; 32], &[]));
        let len = image.len();
        image[len - FOOTER_LEN - 20] ^= 0x01;
        assert!(matches!(extract(&image), Err(ReadError::Corrupt(_))));
    }
}
