use std::{
    collections::HashMap,
    hash::{BuildHasherDefault, Hasher},
};

pub type U64Map<K, V> = HashMap<K, V, BuildHasherDefault<U64Mixer>>;

/// SplitMix64 finalizer (Stafford's Mix13) specialised for single-`u64` keys.
///
/// Only `write_u64` is implemented — any other `write_*` path is a bug,
/// because the keys this map uses (`OrderId`, `Price`) are both single-u64
/// newtypes whose `#[derive(Hash)]` forwards to `write_u64`. If a future
/// key gains additional fields, the derive will switch to per-field
/// `write_*` calls and the `unreachable!` here will fire — that's the
/// signal to either add a real `write` impl or stop using this hasher.
#[derive(Default)]
pub struct U64Mixer(u64);

impl Hasher for U64Mixer {
    #[inline(always)]
    fn write(&mut self, _: &[u8]) {
        unreachable!("U64Mixer is u64-only; key Hash impl must call write_u64")
    }

    #[inline(always)]
    fn write_u64(&mut self, n: u64) {
        let mut x = n;
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58476d1ce4e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d049bb133111eb);
        x ^= x >> 31;
        self.0 = x;
    }
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }
}
