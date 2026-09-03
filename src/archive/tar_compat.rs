use std::io::{self, Read};

const BLOCK: usize = 512;

/// Normalizes nonzero link sizes emitted by R's internal tar implementation.
pub(super) struct LinkSizeFix<R> {
    inner: R,
    block: [u8; BLOCK],
    position: usize,
    length: usize,
    data_remaining: u64,
    eof: bool,
}

impl<R: Read> LinkSizeFix<R> {
    pub(super) fn new(inner: R) -> Self {
        Self {
            inner,
            block: [0; BLOCK],
            position: 0,
            length: 0,
            data_remaining: 0,
            eof: false,
        }
    }

    fn fill_header(&mut self) -> io::Result<()> {
        self.position = 0;
        self.length = 0;
        while self.length < BLOCK {
            let count = self.inner.read(&mut self.block[self.length..])?;
            if count == 0 {
                self.eof = true;
                break;
            }
            self.length += count;
        }
        if self.length == BLOCK {
            self.data_remaining = self.normalize();
        }
        Ok(())
    }

    fn normalize(&mut self) -> u64 {
        if self.block.iter().all(|byte| *byte == 0) {
            return 0;
        }
        let Some(size) = parse_tar_number(&self.block[124..136]) else {
            return 0;
        };
        if matches!(self.block[156], b'1' | b'2') && size != 0 {
            self.block[124..136].fill(b'0');
            self.block[135] = 0;
            let checksum = self
                .block
                .iter()
                .enumerate()
                .map(|(index, byte)| {
                    if (148..156).contains(&index) {
                        b' '
                    } else {
                        *byte
                    }
                })
                .map(u64::from)
                .sum::<u64>();
            let encoded = format!("{checksum:06o}\0 ");
            self.block[148..156].copy_from_slice(encoded.as_bytes());
            0
        } else {
            size.div_ceil(BLOCK as u64) * BLOCK as u64
        }
    }
}

impl<R: Read> Read for LinkSizeFix<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.position < self.length {
            let count = (self.length - self.position).min(output.len());
            output[..count].copy_from_slice(&self.block[self.position..self.position + count]);
            self.position += count;
            return Ok(count);
        }
        if self.data_remaining != 0 {
            let wanted = self.data_remaining.min(output.len() as u64) as usize;
            let count = self.inner.read(&mut output[..wanted])?;
            self.data_remaining -= count as u64;
            self.eof |= count == 0;
            return Ok(count);
        }
        if self.eof {
            return Ok(0);
        }
        self.fill_header()?;
        if self.length == 0 {
            return Ok(0);
        }
        let count = self.length.min(output.len());
        output[..count].copy_from_slice(&self.block[..count]);
        self.position = count;
        Ok(count)
    }
}

fn parse_tar_number(field: &[u8]) -> Option<u64> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        return field
            .iter()
            .enumerate()
            .try_fold(0_u64, |value, (index, byte)| {
                value
                    .checked_mul(256)?
                    .checked_add(u64::from(if index == 0 { byte & 0x7f } else { *byte }))
            });
    }
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    let value = std::str::from_utf8(&field[..end]).ok()?.trim();
    if value.is_empty() {
        Some(0)
    } else {
        u64::from_str_radix(value, 8).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_r_style_symlink_size() {
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg/link").unwrap();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name("target").unwrap();
        header.set_size(1234);
        header.set_cksum();
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend([0_u8; BLOCK * 2]);
        let mut fixed = Vec::new();
        LinkSizeFix::new(bytes.as_slice())
            .read_to_end(&mut fixed)
            .unwrap();
        assert_eq!(parse_tar_number(&fixed[124..136]), Some(0));
    }
}
