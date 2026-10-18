pub const UAB_SIGNATURE_SECTION: &str = ".note.uab.sig";
pub const UAB_META_SECTION: &str = "linglong.meta";
pub const UAB_SIGNATURE_DIGEST_SIZE: usize = 64;
pub const UAB_SIGNATURE_DIGEST_OFFSET: usize = 28;
pub const UAB_SIGNATURE_NOTE_SIZE: usize = 92;

#[derive(Clone, Copy)]
pub(crate) enum SignatureEndian {
    Little,
    Big,
}

impl SignatureEndian {
    fn read_u32(self, bytes: &[u8]) -> u32 {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        }
    }
}

pub const UAB_SIGNATURE_NOTE: [u8; UAB_SIGNATURE_NOTE_SIZE] = signature_note();

const fn signature_note() -> [u8; UAB_SIGNATURE_NOTE_SIZE] {
    let mut note = [0_u8; UAB_SIGNATURE_NOTE_SIZE];
    let name_size = 14_u32.to_ne_bytes();
    let digest_size = (UAB_SIGNATURE_DIGEST_SIZE as u32).to_ne_bytes();
    let note_type = 1_u32.to_ne_bytes();
    let mut index = 0;
    while index < 4 {
        note[index] = name_size[index];
        note[4 + index] = digest_size[index];
        note[8 + index] = note_type[index];
        index += 1;
    }
    let name = b"linglong.meta\0";
    index = 0;
    while index < name.len() {
        note[12 + index] = name[index];
        index += 1;
    }
    note[UAB_SIGNATURE_DIGEST_OFFSET] = b'!';
    note
}

pub(crate) fn parse_signature_note(data: &[u8], endian: SignatureEndian) -> Option<&[u8]> {
    if data.len() != UAB_SIGNATURE_NOTE_SIZE
        || endian.read_u32(&data[0..4]) != 14
        || endian.read_u32(&data[4..8]) != UAB_SIGNATURE_DIGEST_SIZE as u32
        || endian.read_u32(&data[8..12]) != 1
        || &data[12..26] != b"linglong.meta\0"
        || data[26..28] != [0, 0]
    {
        return None;
    }
    Some(&data[UAB_SIGNATURE_DIGEST_OFFSET..])
}

pub(crate) fn is_sha256_digest(digest: &[u8]) -> bool {
    digest.len() == UAB_SIGNATURE_DIGEST_SIZE
        && digest
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_note_layout_matches_upstream_abi() {
        let endian = if cfg!(target_endian = "little") {
            SignatureEndian::Little
        } else {
            SignatureEndian::Big
        };
        let digest = parse_signature_note(&UAB_SIGNATURE_NOTE, endian).unwrap();
        assert_eq!(digest.len(), UAB_SIGNATURE_DIGEST_SIZE);
        assert_eq!(digest[0], b'!');
    }
}
