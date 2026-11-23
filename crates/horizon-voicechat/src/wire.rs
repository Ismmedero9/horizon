//! Minecraft's byte encoding as Simple Voice Chat uses it (`FriendlyByteBuf`): big-endian numbers,
//! a UUID as two longs (most significant first), a string as a VarInt byte length and UTF-8.

#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    Short,
    VarIntTooLong,
    BadString,
    TooLong,
}

pub struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self { Self { data, at: 0 } }
    pub fn left(&self) -> usize { self.data.len() - self.at }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.left() < n { return Err(WireError::Short); }
        let s = &self.data[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    pub fn rest(&mut self) -> &'a [u8] { let s = &self.data[self.at..]; self.at = self.data.len(); s }
    pub fn u8(&mut self) -> Result<u8, WireError> { Ok(self.take(1)?[0]) }
    pub fn bool(&mut self) -> Result<bool, WireError> { Ok(self.u8()? != 0) }
    pub fn i32(&mut self) -> Result<i32, WireError> { Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap())) }
    pub fn i64(&mut self) -> Result<i64, WireError> { Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap())) }
    pub fn f32(&mut self) -> Result<f32, WireError> { Ok(f32::from_be_bytes(self.take(4)?.try_into().unwrap())) }
    pub fn f64(&mut self) -> Result<f64, WireError> { Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap())) }
    pub fn uuid(&mut self) -> Result<u128, WireError> { Ok(u128::from_be_bytes(self.take(16)?.try_into().unwrap())) }
    pub fn array16(&mut self) -> Result<[u8; 16], WireError> { Ok(self.take(16)?.try_into().unwrap()) }
    pub fn varint(&mut self) -> Result<u32, WireError> {
        let mut v: u32 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            v |= ((b & 0x7F) as u32) << (7 * i);
            if b & 0x80 == 0 { return Ok(v); }
        }
        Err(WireError::VarIntTooLong)
    }
    /// A length-prefixed byte array of at most `max` bytes.
    pub fn bytes(&mut self, max: usize) -> Result<&'a [u8], WireError> {
        let n = self.varint()? as usize;
        if n > max { return Err(WireError::TooLong); }
        self.take(n)
    }
    /// A string of at most `max` characters (the byte limit is four times that).
    pub fn string(&mut self, max_chars: usize) -> Result<String, WireError> {
        let b = self.bytes(max_chars * 4)?;
        let s = std::str::from_utf8(b).map_err(|_| WireError::BadString)?;
        if s.chars().count() > max_chars { return Err(WireError::TooLong); }
        Ok(s.to_owned())
    }
}

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn new() -> Self { Self(Vec::new()) }
    pub fn u8(&mut self, v: u8) -> &mut Self { self.0.push(v); self }
    pub fn bool(&mut self, v: bool) -> &mut Self { self.u8(v as u8) }
    pub fn i32(&mut self, v: i32) -> &mut Self { self.0.extend_from_slice(&v.to_be_bytes()); self }
    pub fn i64(&mut self, v: i64) -> &mut Self { self.0.extend_from_slice(&v.to_be_bytes()); self }
    pub fn f32(&mut self, v: f32) -> &mut Self { self.0.extend_from_slice(&v.to_be_bytes()); self }
    pub fn f64(&mut self, v: f64) -> &mut Self { self.0.extend_from_slice(&v.to_be_bytes()); self }
    pub fn uuid(&mut self, v: u128) -> &mut Self { self.0.extend_from_slice(&v.to_be_bytes()); self }
    pub fn raw(&mut self, v: &[u8]) -> &mut Self { self.0.extend_from_slice(v); self }
    pub fn varint(&mut self, mut v: u32) -> &mut Self {
        loop {
            if v & !0x7F == 0 { self.0.push(v as u8); return self; }
            self.0.push((v & 0x7F) as u8 | 0x80);
            v >>= 7;
        }
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self { self.varint(v.len() as u32).raw(v) }
    pub fn string(&mut self, v: &str) -> &mut Self { self.bytes(v.as_bytes()) }
    pub fn finish(self) -> Vec<u8> { self.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_and_match_minecrafts_bytes() {
        for v in [0u32, 1, 127, 128, 255, 25565, 2097151, u32::MAX] {
            let mut w = Writer::new();
            w.varint(v);
            assert_eq!(Reader::new(&w.0).varint().unwrap(), v);
        }
        // Minecraft's documented examples: 128 is 0x80 0x01, 25565 is 0xDD 0xC7 0x01.
        assert_eq!({ let mut w = Writer::new(); w.varint(128); w.0 }, vec![0x80, 0x01]);
        assert_eq!({ let mut w = Writer::new(); w.varint(25565); w.0 }, vec![0xDD, 0xC7, 0x01]);
        assert_eq!(Reader::new(&[0x80; 6]).varint(), Err(WireError::VarIntTooLong));
    }

    #[test]
    fn numbers_uuids_and_strings_round_trip() {
        let mut w = Writer::new();
        w.i32(-7).i64(1 << 40).f32(1.5).f64(-2.25).uuid(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff).string("héllo").bool(true);
        let mut r = Reader::new(&w.0);
        assert_eq!(r.i32().unwrap(), -7);
        assert_eq!(r.i64().unwrap(), 1 << 40);
        assert_eq!(r.f32().unwrap(), 1.5);
        assert_eq!(r.f64().unwrap(), -2.25);
        assert_eq!(r.uuid().unwrap(), 0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
        assert_eq!(r.string(32).unwrap(), "héllo");
        assert!(r.bool().unwrap());
        assert_eq!(r.left(), 0);
        assert_eq!(r.u8(), Err(WireError::Short));
    }

    #[test]
    fn the_uuid_is_two_longs_most_significant_first() {
        let mut w = Writer::new();
        w.uuid(0x0102030405060708_090a0b0c0d0e0f10);
        assert_eq!(w.0, (1..=16).collect::<Vec<u8>>());
    }

    #[test]
    fn limits_are_enforced() {
        let mut w = Writer::new();
        w.string("abcdef");
        assert_eq!(Reader::new(&w.0).string(5), Err(WireError::TooLong));
        let mut w = Writer::new();
        w.bytes(&[0; 10]);
        assert_eq!(Reader::new(&w.0).bytes(9), Err(WireError::TooLong));
    }
}
